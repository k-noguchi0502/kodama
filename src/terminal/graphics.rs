//! Kitty graphics protocol: transmitting, placing and deleting images.
//!
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/>. Images are stored
//! decoded (RGBA) per screen; placements anchor an image to an absolute
//! history line so they scroll with the text around them.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

/// Total decoded image data kept per screen before old images are evicted.
const STORAGE_QUOTA: usize = 256 * 1024 * 1024;
/// Largest single image accepted, in bytes of payload or decoded pixels.
const MAX_IMAGE_BYTES: usize = 200 * 1024 * 1024;

/// Identifies decoded pixel data across terminals, for the renderer's
/// texture cache.
static NEXT_UID: AtomicU64 = AtomicU64::new(1);

pub struct Image {
    pub uid: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    number: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub image: u32,
    pub id: u32,
    /// Absolute line of the top-left cell (see [`crate::terminal::Terminal::abs_line`]).
    pub line: u64,
    pub col: usize,
    /// Source rectangle in image pixels: x, y, width, height.
    pub src: [u32; 4],
    /// Requested size in cells (0 = derived from the pixel size).
    pub fit: (u32, u32),
    /// Pixel offset inside the first cell.
    pub offset: (u32, u32),
    /// Cells covered, computed when placed.
    pub cells: (usize, usize),
    pub z: i32,
}

impl Placement {
    /// Size on screen in pixels for the given cell size.
    pub fn size(&self, cell: (f32, f32)) -> (f32, f32) {
        let (sw, sh) = (self.src[2] as f32, self.src[3] as f32);
        match self.fit {
            (0, 0) => (sw, sh),
            (c, 0) => {
                let w = c as f32 * cell.0;
                (w, sh * w / sw.max(1.0))
            }
            (0, r) => {
                let h = r as f32 * cell.1;
                (sw * h / sh.max(1.0), h)
            }
            (c, r) => (c as f32 * cell.0, r as f32 * cell.1),
        }
    }

    fn intersects(&self, line: u64, col: usize) -> bool {
        self.covers_line(line) && (self.col..self.col + self.cells.0).contains(&col)
    }

    fn covers_line(&self, line: u64) -> bool {
        (self.line..self.line + self.cells.1 as u64).contains(&line)
    }
}

/// Where the command arrived, in terms of the screen.
pub struct Context {
    pub cursor_line: u64,
    pub cursor_col: usize,
    /// Absolute line of the top screen row.
    pub screen_top: u64,
    pub rows: usize,
    pub cell: (f32, f32),
}

/// What the terminal must do after a command.
#[derive(Default, Debug, PartialEq)]
pub struct Outcome {
    pub response: Option<String>,
    /// Cells covered by a new placement the cursor should move past.
    pub move_cursor: Option<(usize, usize)>,
}

#[derive(Default)]
pub struct Graphics {
    images: HashMap<u32, Image>,
    pub placements: Vec<Placement>,
    /// First command and base64 payload of a chunked transmission.
    pending: Option<(Command, Vec<u8>)>,
    next_id: u32,
    bytes: usize,
}

#[derive(Clone, Default, Debug)]
struct Command {
    action: u8,
    format: u32,
    medium: u8,
    compression: u8,
    width: u32,
    height: u32,
    size: usize,
    offset: usize,
    id: u32,
    number: u32,
    placement: u32,
    more: bool,
    quiet: u32,
    src: [u32; 4],
    fit: (u32, u32),
    cell_offset: (u32, u32),
    no_cursor_move: bool,
    z: i32,
    delete: u8,
}

impl Command {
    fn parse(control: &[u8]) -> Self {
        let mut cmd = Command::default();
        for pair in control.split(|&b| b == b',') {
            let [key, b'=', value @ ..] = pair else {
                continue;
            };
            let text = std::str::from_utf8(value).unwrap_or("");
            let num = || text.parse::<u32>().unwrap_or(0);
            let byte = value.first().copied().unwrap_or(0);
            match key {
                b'a' => cmd.action = byte,
                b'f' => cmd.format = num(),
                b't' => cmd.medium = byte,
                b'o' => cmd.compression = byte,
                b's' => cmd.width = num(),
                b'v' => cmd.height = num(),
                b'S' => cmd.size = num() as usize,
                b'O' => cmd.offset = num() as usize,
                b'i' => cmd.id = num(),
                b'I' => cmd.number = num(),
                b'p' => cmd.placement = num(),
                b'm' => cmd.more = num() == 1,
                b'q' => cmd.quiet = num(),
                b'x' => cmd.src[0] = num(),
                b'y' => cmd.src[1] = num(),
                b'w' => cmd.src[2] = num(),
                b'h' => cmd.src[3] = num(),
                b'c' => cmd.fit.0 = num(),
                b'r' => cmd.fit.1 = num(),
                b'X' => cmd.cell_offset.0 = num(),
                b'Y' => cmd.cell_offset.1 = num(),
                b'C' => cmd.no_cursor_move = num() == 1,
                b'z' => cmd.z = text.parse().unwrap_or(0),
                b'd' => cmd.delete = byte,
                _ => {}
            }
        }
        cmd
    }

    fn reply(&self, result: Result<(), String>) -> Option<String> {
        if self.id == 0 && self.number == 0 {
            return None;
        }
        let message = match result {
            Ok(()) if self.quiet >= 1 => return None,
            Err(_) if self.quiet >= 2 => return None,
            Ok(()) => "OK".to_string(),
            Err(e) => e,
        };
        let mut out = format!("\x1b_Gi={}", self.id);
        if self.number != 0 {
            out.push_str(&format!(",I={}", self.number));
        }
        if self.placement != 0 {
            out.push_str(&format!(",p={}", self.placement));
        }
        out.push_str(&format!(";{message}\x1b\\"));
        Some(out)
    }
}

impl Graphics {
    /// Handles the body of `APC G <control> ; <payload> ST`.
    pub fn command(&mut self, body: &[u8], ctx: &Context) -> Outcome {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &[][..]),
        };
        let mut cmd = Command::parse(control);

        // Continuation chunks carry only `m` (and maybe `q`).
        if let Some((first, mut data)) = self.pending.take() {
            if data.len() + payload.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
                return Outcome {
                    response: first.reply(Err("EFBIG:image too large".into())),
                    ..Default::default()
                };
            }
            data.extend_from_slice(payload);
            if cmd.more {
                self.pending = Some((first, data));
                return Outcome::default();
            }
            let quiet = cmd.quiet.max(first.quiet);
            cmd = first;
            cmd.quiet = quiet;
            return self.run(cmd, &data, ctx);
        }
        if cmd.more && matches!(cmd.action, 0 | b't' | b'T' | b'q') {
            self.pending = Some((cmd, payload.to_vec()));
            return Outcome::default();
        }
        self.run(cmd, payload, ctx)
    }

    fn run(&mut self, mut cmd: Command, payload: &[u8], ctx: &Context) -> Outcome {
        match cmd.action {
            0 | b't' | b'T' | b'q' => {
                let image = load(&cmd, payload);
                if cmd.action == b'q' {
                    return Outcome {
                        response: cmd.reply(image.map(drop)),
                        ..Default::default()
                    };
                }
                let image = match image {
                    Ok(image) => image,
                    Err(e) => {
                        return Outcome {
                            response: cmd.reply(Err(e)),
                            ..Default::default()
                        };
                    }
                };
                if cmd.id == 0 {
                    cmd.id = self.allocate_id();
                }
                self.store(cmd.id, cmd.number, image);
                if cmd.action == b'T' {
                    return self.place(&cmd, ctx);
                }
                Outcome {
                    response: cmd.reply(Ok(())),
                    ..Default::default()
                }
            }
            b'p' => self.place(&cmd, ctx),
            b'd' => {
                self.delete(&cmd, ctx);
                Outcome::default()
            }
            _ => Outcome::default(),
        }
    }

    fn allocate_id(&mut self) -> u32 {
        loop {
            self.next_id = self.next_id.wrapping_add(1).max(1 << 24);
            if !self.images.contains_key(&self.next_id) {
                return self.next_id;
            }
        }
    }

    fn store(&mut self, id: u32, number: u32, rgba: Decoded) {
        if let Some(old) = self.images.remove(&id) {
            self.bytes -= old.rgba.len();
            self.placements.retain(|p| p.image != id);
        }
        let size = rgba.rgba.len();
        while self.bytes + size > STORAGE_QUOTA && self.evict_oldest() {}
        self.bytes += size;
        self.images.insert(
            id,
            Image {
                uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
                width: rgba.width,
                height: rgba.height,
                rgba: rgba.rgba,
                number,
            },
        );
    }

    /// Frees the oldest image, preferring ones that are not on screen.
    fn evict_oldest(&mut self) -> bool {
        let placed = |id: &u32| self.placements.iter().any(|p| p.image == *id);
        let victim = self
            .images
            .iter()
            .filter(|(id, _)| !placed(id))
            .min_by_key(|(_, img)| img.uid)
            .or_else(|| self.images.iter().min_by_key(|(_, img)| img.uid))
            .map(|(id, _)| *id);
        match victim {
            Some(id) => {
                self.remove_image(id);
                true
            }
            None => false,
        }
    }

    fn remove_image(&mut self, id: u32) {
        if let Some(img) = self.images.remove(&id) {
            self.bytes -= img.rgba.len();
        }
        self.placements.retain(|p| p.image != id);
    }

    fn find(&self, cmd: &Command) -> Option<u32> {
        if cmd.id != 0 {
            return self.images.contains_key(&cmd.id).then_some(cmd.id);
        }
        // The newest image with this number.
        self.images
            .iter()
            .filter(|(_, img)| cmd.number != 0 && img.number == cmd.number)
            .max_by_key(|(_, img)| img.uid)
            .map(|(id, _)| *id)
    }

    fn place(&mut self, cmd: &Command, ctx: &Context) -> Outcome {
        let Some(id) = self.find(cmd) else {
            return Outcome {
                response: cmd.reply(Err("ENOENT:no such image".into())),
                ..Default::default()
            };
        };
        let img = &self.images[&id];
        let [x, y, w, h] = cmd.src;
        let x = x.min(img.width);
        let y = y.min(img.height);
        let w = if w == 0 {
            img.width - x
        } else {
            w.min(img.width - x)
        };
        let h = if h == 0 {
            img.height - y
        } else {
            h.min(img.height - y)
        };
        let mut placement = Placement {
            image: id,
            id: cmd.placement,
            line: ctx.cursor_line,
            col: ctx.cursor_col,
            src: [x, y, w, h],
            fit: cmd.fit,
            offset: cmd.cell_offset,
            cells: (1, 1),
            z: cmd.z,
        };
        let (pw, ph) = placement.size(ctx.cell);
        placement.cells = (
            (((cmd.cell_offset.0 as f32 + pw) / ctx.cell.0).ceil() as usize).max(1),
            (((cmd.cell_offset.1 as f32 + ph) / ctx.cell.1).ceil() as usize).max(1),
        );
        if cmd.placement != 0 {
            self.placements
                .retain(|p| !(p.image == id && p.id == cmd.placement));
        }
        let cells = placement.cells;
        self.placements.push(placement);
        Outcome {
            response: cmd.reply(Ok(())),
            move_cursor: (!cmd.no_cursor_move).then_some(cells),
        }
    }

    fn delete(&mut self, cmd: &Command, ctx: &Context) {
        let target = cmd.delete.to_ascii_lowercase();
        let free = cmd.delete.is_ascii_uppercase();
        let screen = ctx.screen_top..ctx.screen_top + ctx.rows as u64;
        // 1-based cell coordinates relative to the screen.
        let (cx, cy) = (
            cmd.src[0].saturating_sub(1) as usize,
            cmd.src[1].saturating_sub(1) as u64,
        );
        let number_id = (target == b'n')
            .then(|| {
                self.find(&Command {
                    id: 0,
                    ..cmd.clone()
                })
            })
            .flatten();
        let doomed = |p: &Placement| match target {
            0 | b'a' => (p.line..p.line + p.cells.1 as u64).any(|l| screen.contains(&l)),
            b'i' => p.image == cmd.id && (cmd.placement == 0 || p.id == cmd.placement),
            b'n' => Some(p.image) == number_id && (cmd.placement == 0 || p.id == cmd.placement),
            b'c' => p.intersects(ctx.cursor_line, ctx.cursor_col),
            b'p' => p.intersects(ctx.screen_top + cy, cx),
            b'q' => p.intersects(ctx.screen_top + cy, cx) && p.z == cmd.z,
            b'x' => (p.col..p.col + p.cells.0).contains(&cx),
            b'y' => p.covers_line(ctx.screen_top + cy),
            b'z' => p.z == cmd.z,
            b'r' => (cmd.src[0]..=cmd.src[1]).contains(&p.image),
            _ => false,
        };
        let mut touched = Vec::new();
        self.placements.retain(|p| {
            let gone = doomed(p);
            if gone {
                touched.push(p.image);
            }
            !gone
        });
        if free {
            match target {
                b'i' => touched.push(cmd.id),
                b'n' => touched.extend(number_id),
                b'r' => touched.extend(
                    self.images
                        .keys()
                        .filter(|id| (cmd.src[0]..=cmd.src[1]).contains(id)),
                ),
                _ => {}
            }
            for id in touched {
                if !self.placements.iter().any(|p| p.image == id) {
                    self.remove_image(id);
                }
            }
        }
    }

    pub fn image(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.pending.is_none()
    }

    /// Removes placements touching absolute lines in `range`.
    pub fn clear_lines(&mut self, range: std::ops::Range<u64>) {
        self.placements
            .retain(|p| !(p.line..p.line + p.cells.1 as u64).any(|l| range.contains(&l)));
    }

    /// Drops placements that ended above `first_line` (fell out of history).
    pub fn forget_before(&mut self, first_line: u64) {
        self.placements
            .retain(|p| p.line + p.cells.1 as u64 > first_line);
    }

    /// Moves placements anchored inside the line range `region` by `delta`
    /// lines, dropping those pushed out of it (scrolling without history).
    pub fn scroll(&mut self, region: std::ops::Range<u64>, delta: i64) {
        self.placements.retain_mut(|p| {
            if !region.contains(&p.line) {
                return true;
            }
            let moved = p.line as i64 + delta;
            if moved < region.start as i64 || moved >= region.end as i64 {
                return false;
            }
            p.line = moved as u64;
            true
        });
    }

    /// Shifts every placement by `delta` lines.
    pub fn shift(&mut self, delta: i64) {
        self.placements.retain_mut(|p| {
            let moved = p.line as i64 + delta;
            p.line = moved.max(0) as u64;
            moved + p.cells.1 as i64 > 0
        });
    }
}

struct Decoded {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Reads the payload from its medium and decodes it to RGBA.
fn load(cmd: &Command, payload: &[u8]) -> Result<Decoded, String> {
    let data = base64_decode(payload).ok_or("EINVAL:bad base64")?;
    let mut data = match cmd.medium {
        0 | b'd' => data,
        b'f' | b't' | b's' => read_medium(cmd, &data)?,
        _ => return Err("EINVAL:unsupported transmission medium".into()),
    };
    if cmd.compression == b'z' {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(&data[..])
            .take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|_| "EINVAL:bad zlib data")?;
        data = out;
    }
    if data.len() > MAX_IMAGE_BYTES {
        return Err("EFBIG:image too large".into());
    }
    let (w, h) = (cmd.width, cmd.height);
    let raw = |bpp: usize| -> Result<Decoded, String> {
        let len = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(bpp));
        match len {
            Some(len) if w > 0 && h > 0 && len <= data.len() && len <= MAX_IMAGE_BYTES => {
                Ok(Decoded {
                    width: w,
                    height: h,
                    rgba: if bpp == 4 {
                        data[..len].to_vec()
                    } else {
                        data[..len]
                            .as_chunks::<3>()
                            .0
                            .iter()
                            .flat_map(|&[r, g, b]| [r, g, b, 255])
                            .collect()
                    },
                })
            }
            _ => Err("ENODATA:insufficient image data".into()),
        }
    };
    match cmd.format {
        24 => raw(3),
        0 | 32 => raw(4),
        100 => {
            let (rgba, w, h) =
                crate::renderer::font::decode_png(&data).ok_or("EBADPNG:invalid PNG")?;
            Ok(Decoded {
                width: w as u32,
                height: h as u32,
                rgba,
            })
        }
        _ => Err("EINVAL:unknown format".into()),
    }
}

/// Image data passed as a file (`t=f`), temporary file (`t=t`) or POSIX
/// shared memory object (`t=s`). `name` is the decoded payload.
fn read_medium(cmd: &Command, name: &[u8]) -> Result<Vec<u8>, String> {
    let name = std::str::from_utf8(name).map_err(|_| "EINVAL:bad file name")?;
    let path = match cmd.medium {
        b's' => format!("/dev/shm/{}", name.trim_start_matches('/')),
        _ => name.to_string(),
    };
    let denied = ["/proc/", "/sys/", "/dev/"];
    if cmd.medium != b's' && denied.iter().any(|d| path.starts_with(d)) {
        return Err("EPERM:refusing to read device or kernel files".into());
    }
    let meta = std::fs::metadata(&path).map_err(|e| format!("EBADF:{e}"))?;
    if !meta.is_file() {
        return Err("EBADF:not a regular file".into());
    }
    let mut file = std::fs::File::open(&path).map_err(|e| format!("EBADF:{e}"))?;
    if cmd.offset > 0 {
        std::io::copy(
            &mut (&mut file).take(cmd.offset as u64),
            &mut std::io::sink(),
        )
        .map_err(|e| format!("EBADF:{e}"))?;
    }
    let limit = if cmd.size > 0 {
        cmd.size
    } else {
        MAX_IMAGE_BYTES + 1
    };
    let mut data = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut data)
        .map_err(|e| format!("EBADF:{e}"))?;

    let temp_dir = std::env::temp_dir();
    let is_temp = path.starts_with("/tmp/")
        || path.starts_with("/dev/shm/")
        || std::path::Path::new(&path).starts_with(&temp_dir);
    if cmd.medium == b's'
        || (cmd.medium == b't' && is_temp && path.contains("tty-graphics-protocol"))
    {
        let _ = std::fs::remove_file(&path);
    }
    Ok(data)
}

pub(crate) fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in input {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            cursor_line: 5,
            cursor_col: 2,
            screen_top: 0,
            rows: 24,
            cell: (10.0, 20.0),
        }
    }

    fn b64(data: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
            out.push_str(&"=".repeat(3 - chunk.len()));
        }
        out
    }

    #[test]
    fn base64_round_trip() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"\x00\xff\x10kodama"] {
            assert_eq!(base64_decode(b64(data).as_bytes()).unwrap(), data);
        }
    }

    #[test]
    fn transmit_and_display_places_at_the_cursor() {
        let mut g = Graphics::default();
        let pixels = b64(&[255u8; 4 * 15 * 30]);
        let out = g.command(
            format!("a=T,f=32,s=15,v=30,i=7;{pixels}").as_bytes(),
            &ctx(),
        );
        assert_eq!(out.response.as_deref(), Some("\x1b_Gi=7;OK\x1b\\"));
        // 15x30 px over 10x20 px cells.
        assert_eq!(out.move_cursor, Some((2, 2)));
        let p = &g.placements[0];
        assert_eq!((p.image, p.line, p.col, p.src), (7, 5, 2, [0, 0, 15, 30]));
        assert_eq!(g.image(7).unwrap().rgba.len(), 4 * 15 * 30);
    }

    #[test]
    fn chunked_rgb_with_explicit_cells() {
        let mut g = Graphics::default();
        let pixels = b64(&[1u8, 2, 3, 4, 5, 6]);
        let (a, b) = pixels.split_at(4);
        assert_eq!(
            g.command(
                format!("a=T,f=24,s=2,v=1,c=4,r=1,q=1,m=1;{a}").as_bytes(),
                &ctx()
            ),
            Outcome::default()
        );
        let out = g.command(format!("m=0;{b}").as_bytes(), &ctx());
        assert_eq!(out.move_cursor, Some((4, 1)));
        let img = g.image(g.placements[0].image).unwrap();
        assert_eq!(img.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(g.placements[0].size((10.0, 20.0)), (40.0, 20.0));
    }

    #[test]
    fn query_answers_without_storing() {
        let mut g = Graphics::default();
        let out = g.command(b"i=31,s=1,v=1,a=q,t=d,f=24;AAAA", &ctx());
        assert_eq!(out.response.as_deref(), Some("\x1b_Gi=31;OK\x1b\\"));
        assert!(g.is_empty());
        let out = g.command(b"i=32,s=2,v=2,a=q,f=24;AAAA", &ctx());
        assert!(out.response.unwrap().contains("ENODATA"));
    }

    #[test]
    fn delete_by_id_frees_data_only_when_uppercase() {
        let mut g = Graphics::default();
        let pixels = b64(&[0u8; 4]);
        g.command(format!("a=T,s=1,v=1,i=1,q=2;{pixels}").as_bytes(), &ctx());
        g.command(b"a=d,d=i,i=1", &ctx());
        assert!(g.placements.is_empty());
        assert!(g.image(1).is_some());
        g.command(b"a=p,i=1,q=2", &ctx());
        g.command(b"a=d,d=I,i=1", &ctx());
        assert!(g.image(1).is_none());
    }

    #[test]
    fn scrolling_without_history_moves_and_drops_placements() {
        let mut g = Graphics::default();
        let pixels = b64(&[0u8; 4]);
        g.command(format!("a=T,s=1,v=1,i=1,q=2;{pixels}").as_bytes(), &ctx());
        g.scroll(0..24, -3);
        assert_eq!(g.placements[0].line, 2);
        g.scroll(0..24, -3);
        assert!(g.placements.is_empty());
    }
}
