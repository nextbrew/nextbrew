//! Read and rewrite the path strings in a Mach-O file's load commands.
//!
//! Relocating a bottle means changing a library's install name, the paths of
//! the libraries a file loads and its rpaths. `install_name_tool` rewrites the
//! whole file for every change. The strings live in the load commands at the
//! start of the file, NUL-terminated and padded to an 8-byte boundary, so a
//! replacement that fits is a handful of bytes written in place. One that
//! does not grows its command into the padding the linker leaves before the
//! first section, as `install_name_tool` and Homebrew's ruby-macho do.
//! The file keeps its length; the caller only has to re-sign it.

use std::fmt;
use std::ops::Range;

use object::macho;

/// Which load command a path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathKind {
    /// `LC_ID_DYLIB`: the library's own install name.
    InstallName,
    /// `LC_LOAD_DYLIB` and its weak, re-export, upward and lazy variants: a
    /// library this file loads.
    Dylib,
    /// `LC_RPATH`.
    Rpath,
}

/// A path string in a load command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadPath {
    pub(crate) kind: PathKind,
    /// The path, lossily decoded. Paths in bottles are ASCII.
    pub(crate) value: String,
    raw: Vec<u8>,
    /// Offset of the string in the file.
    offset: usize,
    /// Bytes available for the string and its NUL terminator.
    capacity: usize,
    /// Offset of the load command in the file, and its size.
    command: usize,
    cmdsize: usize,
    /// Index of the architecture slice, in [`MachoInfo::layouts`].
    slice: usize,
}

/// Where one architecture's load commands are and how far they may grow.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SliceLayout {
    start: usize,
    endian: Endian,
    header_size: usize,
    sizeofcmds: usize,
    /// Commands are padded to this: 8 bytes in 64-bit files, 4 in 32-bit.
    align: usize,
    /// Offset of the first section data in the file: the commands may grow
    /// up to here (ruby-macho's `low_fileoff`).
    low_fileoff: usize,
}

/// What the load commands of a file say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachoInfo {
    pub(crate) paths: Vec<LoadPath>,
    /// Whether every architecture in the file carries an `LC_CODE_SIGNATURE`.
    pub(crate) signed: bool,
    /// Whether some architecture is an executable, dylib or bundle, the
    /// kinds `codesign` signs. Objects and dSYMs are left unsigned.
    pub(crate) signable: bool,
    layouts: Vec<SliceLayout>,
}

/// A change that did not fit in the load command or the header padding.
/// Homebrew cannot apply it either: ruby-macho raises `HeaderPadError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnfitChange {
    pub(crate) kind: PathKind,
    pub(crate) old: String,
    pub(crate) new: String,
}

/// The result of rewriting a file's load paths in place.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Rewrite {
    /// Byte ranges that were changed, so the caller can write only those.
    pub(crate) written: Vec<Range<usize>>,
    pub(crate) unfit: Vec<UnfitChange>,
    /// Whether the file was signed before the rewrite; see [`MachoInfo`].
    pub(crate) signed: bool,
    pub(crate) signable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MachoError {
    /// The data does not start with a Mach-O or fat magic number.
    NotMacho,
    /// A header, load command or fat arch points past the end of the data.
    Truncated(&'static str),
    /// A load command has an impossible size or string offset.
    BadLoadCommand { index: u32, cmd: u32 },
    /// A path string runs to the end of its load command without a NUL.
    UnterminatedString { index: u32, cmd: u32 },
}

impl fmt::Display for MachoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMacho => write!(f, "not a Mach-O file"),
            Self::Truncated(what) => write!(f, "truncated Mach-O: {what} past end of file"),
            Self::BadLoadCommand { index, cmd } => {
                write!(f, "malformed load command {index} (cmd {cmd:#x})")
            }
            Self::UnterminatedString { index, cmd } => {
                write!(
                    f,
                    "unterminated path in load command {index} (cmd {cmd:#x})"
                )
            }
        }
    }
}

impl std::error::Error for MachoError {}

/// Whether `head` (at least 4 bytes) starts with a thin or fat Mach-O magic.
pub(crate) fn has_macho_magic(head: &[u8]) -> bool {
    let Some(magic) = read_u32(head, 0, Endian::Big) else {
        return false;
    };
    // A Java class file starts `CAFEBABE` too, then a major version of 45
    // or more where a fat file has its (few) architectures.
    if magic == macho::FAT_MAGIC {
        return read_u32(head, 4, Endian::Big).is_none_or(|count| count < 45);
    }
    matches!(
        magic,
        macho::MH_MAGIC
            | macho::MH_CIGAM
            | macho::MH_MAGIC_64
            | macho::MH_CIGAM_64
            | macho::FAT_CIGAM
            | macho::FAT_MAGIC_64
            | macho::FAT_CIGAM_64
    )
}

/// Read the load paths of a thin or fat Mach-O file.
pub(crate) fn inspect(data: &[u8]) -> Result<MachoInfo, MachoError> {
    let mut info = MachoInfo {
        paths: Vec::new(),
        signed: true,
        signable: false,
        layouts: Vec::new(),
    };
    for slice in slices(data)? {
        parse_slice(data, slice, &mut info)?;
    }
    Ok(info)
}

/// Replace load paths in place. `map` returns the new path for a kind and
/// current value, or `None` to leave it alone. A replacement that fits is
/// written and NUL-padded, one that does not grows its command into the
/// header padding, and one the padding cannot take is reported in `unfit`
/// with the file left as it was for that command.
pub(crate) fn rewrite(
    data: &mut [u8],
    mut map: impl FnMut(PathKind, &str) -> Option<String>,
) -> Result<Rewrite, MachoError> {
    let MachoInfo {
        paths,
        signed,
        signable,
        mut layouts,
    } = inspect(data)?;
    let mut rewrite = Rewrite {
        signed,
        signable,
        ..Rewrite::default()
    };
    let mut grow_later = Vec::new();
    for path in paths {
        // A path that is not UTF-8 cannot be matched against the prefixes we
        // know, and writing a lossy decoding back would corrupt it.
        let Ok(current) = std::str::from_utf8(&path.raw) else {
            continue;
        };
        let Some(new) = map(path.kind, current) else {
            continue;
        };
        if new == current {
            continue;
        }
        if new.as_bytes().contains(&0) {
            rewrite.unfit.push(UnfitChange {
                kind: path.kind,
                old: current.to_string(),
                new,
            });
            continue;
        }
        if new.len() + 1 > path.capacity {
            grow_later.push((path, new));
            continue;
        }
        let range = path.offset..path.offset + path.capacity;
        let slot = &mut data[range.clone()];
        slot[..new.len()].copy_from_slice(new.as_bytes());
        slot[new.len()..].fill(0);
        rewrite.written.push(range);
    }

    // Last command first, so growing one moves only commands already done.
    grow_later.sort_by_key(|(path, _)| std::cmp::Reverse(path.command));
    for (path, new) in grow_later {
        match grow(data, &mut layouts[path.slice], &path, &new) {
            Some(ranges) => rewrite.written.extend(ranges),
            None => rewrite.unfit.push(UnfitChange {
                kind: path.kind,
                old: String::from_utf8_lossy(&path.raw).into_owned(),
                new,
            }),
        }
    }
    Ok(rewrite)
}

/// Grow the load command holding `path` so `new` fits, moving the commands
/// after it into the header padding, whatever it holds, as ruby-macho does.
/// `None` when the padding is too small.
fn grow(
    data: &mut [u8],
    layout: &mut SliceLayout,
    path: &LoadPath,
    new: &str,
) -> Option<[Range<usize>; 2]> {
    let string_offset = path.offset - path.command;
    let size = (string_offset + new.len() + 1).next_multiple_of(layout.align);
    let delta = size.checked_sub(path.cmdsize)?;
    let commands_end = layout.start + layout.header_size + layout.sizeofcmds;
    let grown_end = commands_end + delta;
    if grown_end > layout.low_fileoff || data.get(commands_end..grown_end).is_none() {
        return None;
    }
    let sizeofcmds = u32::try_from(layout.sizeofcmds + delta).ok()?;

    let command_end = path.command + path.cmdsize;
    data.copy_within(command_end..commands_end, command_end + delta);
    write_u32(data, path.command + 4, size as u32, layout.endian);
    data[path.offset..path.command + size].fill(0);
    data[path.offset..path.offset + new.len()].copy_from_slice(new.as_bytes());
    write_u32(data, layout.start + 20, sizeofcmds, layout.endian);
    layout.sizeofcmds += delta;
    Some([
        layout.start + 20..layout.start + 24,
        path.command..grown_end,
    ])
}

fn write_u32(data: &mut [u8], offset: usize, value: u32, endian: Endian) {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    data[offset..offset + 4].copy_from_slice(&bytes);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

/// One architecture inside the file: `start..end`.
#[derive(Debug, Clone, Copy)]
struct Slice {
    start: usize,
    end: usize,
}

fn read_u32(data: &[u8], offset: usize, endian: Endian) -> Option<u32> {
    let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
    Some(match endian {
        Endian::Little => u32::from_le_bytes(bytes),
        Endian::Big => u32::from_be_bytes(bytes),
    })
}

fn read_u64(data: &[u8], offset: usize, endian: Endian) -> Option<u64> {
    let bytes: [u8; 8] = data.get(offset..offset + 8)?.try_into().ok()?;
    Some(match endian {
        Endian::Little => u64::from_le_bytes(bytes),
        Endian::Big => u64::from_be_bytes(bytes),
    })
}

/// Fat headers never list more architectures than this; a Java class file
/// shares the fat magic and has its version number where the count goes.
const MAX_FAT_ARCHES: u32 = 32;

/// The architecture slices of the file: one for a thin file, one per entry
/// of a fat header.
fn slices(data: &[u8]) -> Result<Vec<Slice>, MachoError> {
    let magic = read_u32(data, 0, Endian::Big).ok_or(MachoError::NotMacho)?;
    let (entry_size, is_fat64) = match magic {
        macho::FAT_MAGIC => (20, false),
        macho::FAT_MAGIC_64 => (32, true),
        _ => {
            if thin_header(data, 0).is_some() {
                return Ok(vec![Slice {
                    start: 0,
                    end: data.len(),
                }]);
            }
            return Err(MachoError::NotMacho);
        }
    };

    let count = read_u32(data, 4, Endian::Big).ok_or(MachoError::Truncated("fat header"))?;
    if count == 0 || count > MAX_FAT_ARCHES {
        return Err(MachoError::NotMacho);
    }
    let mut slices = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let entry = 8 + i * entry_size;
        let (offset, size) = if is_fat64 {
            (
                read_u64(data, entry + 8, Endian::Big),
                read_u64(data, entry + 16, Endian::Big),
            )
        } else {
            (
                read_u32(data, entry + 8, Endian::Big).map(u64::from),
                read_u32(data, entry + 12, Endian::Big).map(u64::from),
            )
        };
        let (Some(offset), Some(size)) = (offset, size) else {
            return Err(MachoError::Truncated("fat arch table"));
        };
        let start = usize::try_from(offset).map_err(|_| MachoError::Truncated("fat arch"))?;
        let end = offset
            .checked_add(size)
            .and_then(|end| usize::try_from(end).ok())
            .filter(|end| *end <= data.len())
            .ok_or(MachoError::Truncated("fat arch"))?;
        if thin_header(data, start).is_none() {
            return Err(MachoError::NotMacho);
        }
        slices.push(Slice { start, end });
    }
    Ok(slices)
}

struct ThinHeader {
    endian: Endian,
    size: usize,
    is64: bool,
}

/// Decode the magic at `offset`. The magic is stored in the file's own byte
/// order, so reading it little-endian tells both the width and the order.
fn thin_header(data: &[u8], offset: usize) -> Option<ThinHeader> {
    let (endian, size) = match read_u32(data, offset, Endian::Little)? {
        macho::MH_MAGIC_64 => (Endian::Little, 32),
        macho::MH_CIGAM_64 => (Endian::Big, 32),
        macho::MH_MAGIC => (Endian::Little, 28),
        macho::MH_CIGAM => (Endian::Big, 28),
        _ => return None,
    };
    Some(ThinHeader {
        endian,
        size,
        is64: size == 32,
    })
}

fn parse_slice(data: &[u8], slice: Slice, info: &mut MachoInfo) -> Result<(), MachoError> {
    let header = thin_header(data, slice.start).ok_or(MachoError::NotMacho)?;
    let endian = header.endian;
    let ncmds = read_u32(data, slice.start + 16, endian).ok_or(MachoError::Truncated("header"))?;
    let sizeofcmds =
        read_u32(data, slice.start + 20, endian).ok_or(MachoError::Truncated("header"))?;

    let mut offset = slice.start + header.size;
    let end = offset
        .checked_add(sizeofcmds as usize)
        .filter(|end| *end <= slice.end)
        .ok_or(MachoError::Truncated("load commands"))?;
    let mut signed = false;
    let slice_index = info.layouts.len();
    let mut low_fileoff = slice.end;

    for index in 0..ncmds {
        let cmd = read_u32(data, offset, endian).ok_or(MachoError::Truncated("load command"))?;
        let cmdsize = read_u32(data, offset + 4, endian)
            .ok_or(MachoError::Truncated("load command"))? as usize;
        if cmdsize < 8 || offset + cmdsize > end {
            return Err(MachoError::BadLoadCommand { index, cmd });
        }

        let kind = match cmd {
            macho::LC_ID_DYLIB => Some(PathKind::InstallName),
            macho::LC_LOAD_DYLIB
            | macho::LC_LOAD_WEAK_DYLIB
            | macho::LC_REEXPORT_DYLIB
            | macho::LC_LOAD_UPWARD_DYLIB
            | macho::LC_LAZY_LOAD_DYLIB => Some(PathKind::Dylib),
            macho::LC_RPATH => Some(PathKind::Rpath),
            macho::LC_CODE_SIGNATURE => {
                signed = true;
                None
            }
            macho::LC_SEGMENT | macho::LC_SEGMENT_64 => {
                if let Some(first) = first_section_data(data, offset, cmdsize, endian) {
                    low_fileoff = low_fileoff.min(slice.start + first);
                }
                None
            }
            _ => None,
        };

        if let Some(kind) = kind {
            // Both `dylib_command` and `rpath_command` keep the string offset
            // right after `cmd` and `cmdsize`.
            let string_offset = read_u32(data, offset + 8, endian)
                .ok_or(MachoError::Truncated("load command"))?
                as usize;
            if string_offset < 12 || string_offset >= cmdsize {
                return Err(MachoError::BadLoadCommand { index, cmd });
            }
            let start = offset + string_offset;
            let capacity = cmdsize - string_offset;
            let slot = &data[start..start + capacity];
            let len = slot
                .iter()
                .position(|b| *b == 0)
                .ok_or(MachoError::UnterminatedString { index, cmd })?;
            let raw = slot[..len].to_vec();
            info.paths.push(LoadPath {
                kind,
                value: String::from_utf8_lossy(&raw).into_owned(),
                raw,
                offset: start,
                capacity,
                command: offset,
                cmdsize,
                slice: slice_index,
            });
        }

        offset += cmdsize;
    }

    info.signed &= signed;
    info.signable |= matches!(
        read_u32(data, slice.start + 12, endian),
        Some(macho::MH_EXECUTE | macho::MH_DYLIB | macho::MH_BUNDLE)
    );
    info.layouts.push(SliceLayout {
        start: slice.start,
        endian,
        header_size: header.size,
        sizeofcmds: sizeofcmds as usize,
        align: if header.is64 { 8 } else { 4 },
        low_fileoff,
    });
    Ok(())
}

/// The lowest file offset (from the start of the slice) of a segment's
/// section data, skipping empty and zero-fill sections, which take no space
/// in the file.
fn first_section_data(
    data: &[u8],
    command: usize,
    cmdsize: usize,
    endian: Endian,
) -> Option<usize> {
    let is64 = read_u32(data, command, endian)? == macho::LC_SEGMENT_64;
    let (nsects_at, header, section, size_at, offset_at, flags_at) = if is64 {
        (64, 72, 80, 40, 48, 64)
    } else {
        (48, 56, 68, 36, 40, 56)
    };
    let nsects = read_u32(data, command + nsects_at, endian)? as usize;
    (0..nsects)
        .map(|i| command + header + i * section)
        .take_while(|at| at + section <= command + cmdsize)
        .filter_map(|at| {
            let size = if is64 {
                read_u64(data, at + size_at, endian)?
            } else {
                u64::from(read_u32(data, at + size_at, endian)?)
            };
            let kind = read_u32(data, at + flags_at, endian)? & macho::SECTION_TYPE;
            let zero_fill = matches!(
                kind,
                macho::S_ZEROFILL | macho::S_GB_ZEROFILL | macho::S_THREAD_LOCAL_ZEROFILL
            );
            let offset = read_u32(data, at + offset_at, endian)? as usize;
            (size > 0 && !zero_fill && offset > 0).then_some(offset)
        })
        .min()
}

const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CSSLOT_ALTERNATE_CODEDIRECTORIES: u32 = 0x1000;
const CSSLOT_SIGNATURESLOT: u32 = 0x10000;
const CS_ADHOC: u32 = 0x2;
const CS_HASHTYPE_SHA256: u8 = 2;
const CS_HASHTYPE_SHA256_TRUNCATED: u8 = 3;

/// Bring an ad-hoc signature up to date with the patched code by rehashing
/// its pages in place, as `codesign` would, without running it. The special
/// slots (requirements, entitlements, Info.plist) cover blobs patching does
/// not touch, so only the page hashes change. Returns the hash bytes that
/// changed, or `None` when the file needs `codesign`: it is unsigned, signed
/// with a certificate, uses SHA-1, or `patched` reaches into the signature.
pub(crate) fn rehash_adhoc(data: &mut [u8], patched: &[Range<usize>]) -> Option<Vec<Range<usize>>> {
    let mut written = Vec::new();
    for slice in slices(data).ok()? {
        let (sig_start, sig_end) = signature_range(data, slice)?;
        if patched
            .iter()
            .any(|r| r.start < sig_end && sig_start < r.end)
        {
            return None;
        }
        let be = |data: &[u8], at: usize| read_u32(data, at, Endian::Big);
        if be(data, sig_start)? != CSMAGIC_EMBEDDED_SIGNATURE {
            return None;
        }
        let mut directories = Vec::new();
        for i in 0..be(data, sig_start + 8)? as usize {
            let kind = be(data, sig_start + 12 + i * 8)?;
            let blob = sig_start + be(data, sig_start + 16 + i * 8)? as usize;
            match kind {
                CSSLOT_CODEDIRECTORY => directories.push(blob),
                k if (CSSLOT_ALTERNATE_CODEDIRECTORIES..CSSLOT_ALTERNATE_CODEDIRECTORIES + 5)
                    .contains(&k) =>
                {
                    directories.push(blob)
                }
                // An empty CMS wrapper is what ad-hoc signing leaves.
                CSSLOT_SIGNATURESLOT if be(data, blob + 4)? > 8 => return None,
                _ => {}
            }
        }
        if directories.is_empty() {
            return None;
        }
        for cd in directories {
            written.extend(rehash_directory(data, slice, cd, sig_end)?);
        }
    }
    Some(written)
}

/// `dataoff..dataoff + datasize` of the slice's `LC_CODE_SIGNATURE`.
fn signature_range(data: &[u8], slice: Slice) -> Option<(usize, usize)> {
    let header = thin_header(data, slice.start)?;
    let endian = header.endian;
    let mut offset = slice.start + header.size;
    for _ in 0..read_u32(data, slice.start + 16, endian)? {
        let cmd = read_u32(data, offset, endian)?;
        let cmdsize = read_u32(data, offset + 4, endian)? as usize;
        if cmd == macho::LC_CODE_SIGNATURE {
            let start = slice.start + read_u32(data, offset + 8, endian)? as usize;
            let end = start + read_u32(data, offset + 12, endian)? as usize;
            return (end <= slice.end).then_some((start, end));
        }
        offset += cmdsize.max(8);
    }
    None
}

fn rehash_directory(
    data: &mut [u8],
    slice: Slice,
    cd: usize,
    sig_end: usize,
) -> Option<Vec<Range<usize>>> {
    use sha2::{Digest, Sha256};
    let be = |data: &[u8], at: usize| read_u32(data, at, Endian::Big);
    if be(data, cd)? != CSMAGIC_CODEDIRECTORY || be(data, cd + 12)? & CS_ADHOC == 0 {
        return None;
    }
    let version = be(data, cd + 8)?;
    let hash_offset = be(data, cd + 16)? as usize;
    let slots = be(data, cd + 28)? as usize;
    let code_limit = be(data, cd + 32)? as usize;
    let header = data.get(cd + 36..cd + 40)?;
    let (hash_size, hash_type, page_shift) = (header[0] as usize, header[1], header[3]);
    let scatter = version >= 0x20100 && be(data, cd + 44)? != 0;
    let limit64 = version >= 0x20300 && read_u64(data, cd + 56, Endian::Big)? != 0;
    if scatter
        || limit64
        || !matches!(hash_type, CS_HASHTYPE_SHA256 | CS_HASHTYPE_SHA256_TRUNCATED)
        || hash_size > 32
        || slice.start + code_limit > slice.end
    {
        return None;
    }
    let page = if page_shift == 0 {
        code_limit.max(1)
    } else {
        1 << page_shift
    };
    if slots != code_limit.div_ceil(page) || cd + hash_offset + slots * hash_size > sig_end {
        return None;
    }

    let code = slice.start..slice.start + code_limit;
    let mut written = Vec::new();
    for i in 0..slots {
        let start = code.start + i * page;
        let digest = Sha256::digest(&data[start..(start + page).min(code.end)]);
        let at = cd + hash_offset + i * hash_size;
        if data[at..at + hash_size] != digest[..hash_size] {
            data[at..at + hash_size].copy_from_slice(&digest[..hash_size]);
            written.push(at..at + hash_size);
        }
    }
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A load command for the test builder.
    enum Cmd<'a> {
        Id(&'a str),
        Load(&'a str),
        Weak(&'a str),
        Rpath(&'a str),
        Signature,
        /// `LC_UUID`-shaped filler with no string.
        Other,
        /// A path with exactly `pad` bytes of slack after its NUL.
        Padded(PathKind, &'a str, usize),
    }

    fn put_u32(out: &mut Vec<u8>, value: u32, endian: Endian) {
        match endian {
            Endian::Little => out.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => out.extend_from_slice(&value.to_be_bytes()),
        }
    }

    /// Build one architecture: header, then the commands, then a byte of
    /// "code" so the file is longer than its load commands.
    fn thin(endian: Endian, is64: bool, cmds: &[Cmd]) -> Vec<u8> {
        let mut body = Vec::new();
        for cmd in cmds {
            let (cmd_id, header_len, path, pad) = match cmd {
                Cmd::Id(p) => (macho::LC_ID_DYLIB, 24, Some(*p), None),
                Cmd::Load(p) => (macho::LC_LOAD_DYLIB, 24, Some(*p), None),
                Cmd::Weak(p) => (macho::LC_LOAD_WEAK_DYLIB, 24, Some(*p), None),
                Cmd::Rpath(p) => (macho::LC_RPATH, 12, Some(*p), None),
                Cmd::Signature => (macho::LC_CODE_SIGNATURE, 16, None, None),
                Cmd::Other => (macho::LC_UUID, 24, None, None),
                Cmd::Padded(PathKind::InstallName, p, pad) => {
                    (macho::LC_ID_DYLIB, 24, Some(*p), Some(*pad))
                }
                Cmd::Padded(PathKind::Dylib, p, pad) => {
                    (macho::LC_LOAD_DYLIB, 24, Some(*p), Some(*pad))
                }
                Cmd::Padded(PathKind::Rpath, p, pad) => (macho::LC_RPATH, 12, Some(*p), Some(*pad)),
            };
            let mut command = Vec::new();
            match path {
                Some(path) => {
                    let string_len = path.len() + 1;
                    let size = match pad {
                        Some(pad) => header_len + string_len + pad,
                        None => (header_len + string_len).next_multiple_of(8),
                    };
                    put_u32(&mut command, cmd_id, endian);
                    put_u32(&mut command, size as u32, endian);
                    put_u32(&mut command, header_len as u32, endian);
                    command.resize(header_len, 0xAA);
                    command.extend_from_slice(path.as_bytes());
                    command.resize(size, 0);
                }
                None => {
                    put_u32(&mut command, cmd_id, endian);
                    put_u32(&mut command, header_len as u32, endian);
                    command.resize(header_len, 0xBB);
                }
            }
            body.extend_from_slice(&command);
        }

        let mut out = Vec::new();
        let magic = if is64 {
            macho::MH_MAGIC_64
        } else {
            macho::MH_MAGIC
        };
        put_u32(&mut out, magic, endian);
        put_u32(&mut out, 0x0100000c, endian); // cputype
        put_u32(&mut out, 0, endian); // cpusubtype
        put_u32(&mut out, 6, endian); // MH_DYLIB
        put_u32(&mut out, cmds.len() as u32, endian);
        put_u32(&mut out, body.len() as u32, endian);
        put_u32(&mut out, 0, endian); // flags
        if is64 {
            put_u32(&mut out, 0, endian); // reserved
        }
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\xCC");
        out
    }

    fn fat(slices: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, macho::FAT_MAGIC, Endian::Big);
        put_u32(&mut out, slices.len() as u32, Endian::Big);
        let mut offset = 8 + 20 * slices.len();
        for slice in slices {
            put_u32(&mut out, 0x0100000c, Endian::Big);
            put_u32(&mut out, 0, Endian::Big);
            put_u32(&mut out, offset as u32, Endian::Big);
            put_u32(&mut out, slice.len() as u32, Endian::Big);
            put_u32(&mut out, 0, Endian::Big);
            offset += slice.len();
        }
        for slice in slices {
            out.extend_from_slice(slice);
        }
        out
    }

    fn values(info: &MachoInfo) -> Vec<(PathKind, &str)> {
        info.paths
            .iter()
            .map(|p| (p.kind, p.value.as_str()))
            .collect()
    }

    #[test]
    fn only_executables_dylibs_and_bundles_are_signable() {
        let dylib = thin(Endian::Little, true, &[Cmd::Other]);
        assert!(inspect(&dylib).unwrap().signable);
        for filetype in [macho::MH_OBJECT, macho::MH_DSYM] {
            let mut file = dylib.clone();
            file[12..16].copy_from_slice(&filetype.to_le_bytes());
            assert!(!inspect(&file).unwrap().signable, "filetype {filetype}");
        }
    }

    #[test]
    fn lists_paths_in_load_command_order() {
        let data = thin(
            Endian::Little,
            true,
            &[
                Cmd::Other,
                Cmd::Id("@@HOMEBREW_PREFIX@@/opt/x/lib/libx.dylib"),
                Cmd::Load("/usr/lib/libSystem.B.dylib"),
                Cmd::Weak("@@HOMEBREW_PREFIX@@/opt/y/lib/liby.dylib"),
                Cmd::Rpath("@loader_path/../lib"),
            ],
        );

        let info = inspect(&data).unwrap();

        assert_eq!(
            values(&info),
            [
                (
                    PathKind::InstallName,
                    "@@HOMEBREW_PREFIX@@/opt/x/lib/libx.dylib"
                ),
                (PathKind::Dylib, "/usr/lib/libSystem.B.dylib"),
                (PathKind::Dylib, "@@HOMEBREW_PREFIX@@/opt/y/lib/liby.dylib"),
                (PathKind::Rpath, "@loader_path/../lib"),
            ]
        );
        assert!(!info.signed);
    }

    #[test]
    fn reads_big_endian_32_bit_files() {
        let data = thin(
            Endian::Big,
            false,
            &[Cmd::Load("/usr/lib/libz.1.dylib"), Cmd::Signature],
        );

        let info = inspect(&data).unwrap();

        assert_eq!(values(&info), [(PathKind::Dylib, "/usr/lib/libz.1.dylib")]);
        assert!(info.signed);
    }

    #[test]
    fn fat_files_list_every_architecture() {
        let arm = thin(Endian::Little, true, &[Cmd::Load("/a"), Cmd::Signature]);
        let intel = thin(Endian::Little, true, &[Cmd::Load("/b")]);
        let data = fat(&[arm, intel]);

        let info = inspect(&data).unwrap();

        assert_eq!(
            values(&info),
            [(PathKind::Dylib, "/a"), (PathKind::Dylib, "/b")]
        );
        assert!(!info.signed, "one unsigned slice makes the file unsigned");
    }

    #[test]
    fn rewrites_in_place_and_pads_with_nul() {
        let old = "@@HOMEBREW_PREFIX@@/opt/x/lib/libx.dylib";
        let mut data = thin(
            Endian::Little,
            true,
            &[Cmd::Id(old), Cmd::Load("/usr/lib/libSystem.B.dylib")],
        );
        let original = data.clone();

        let rewrite = rewrite(&mut data, |kind, path| {
            assert_eq!(
                kind,
                if path == old {
                    PathKind::InstallName
                } else {
                    PathKind::Dylib
                }
            );
            Some(path.replace("@@HOMEBREW_PREFIX@@", "/opt/nextbrew"))
        })
        .unwrap();

        assert!(!rewrite.written.is_empty());
        assert!(rewrite.unfit.is_empty());
        assert_eq!(data.len(), original.len());
        let info = inspect(&data).unwrap();
        assert_eq!(
            values(&info),
            [
                (PathKind::InstallName, "/opt/nextbrew/opt/x/lib/libx.dylib"),
                (PathKind::Dylib, "/usr/lib/libSystem.B.dylib"),
            ]
        );
        // Only the string slot changed; the tail of the slot is NUL.
        assert_eq!(rewrite.written.len(), 1);
        let range = rewrite.written[0].clone();
        let slot = &data[range.clone()];
        let new = "/opt/nextbrew/opt/x/lib/libx.dylib";
        assert_eq!(&slot[..new.len()], new.as_bytes());
        assert!(slot[new.len()..].iter().all(|b| *b == 0));
        assert_eq!(data[..range.start], original[..range.start]);
        assert_eq!(data[range.end..], original[range.end..]);
    }

    #[test]
    fn a_replacement_that_exactly_fits_is_written() {
        // 24 header bytes + "/a/b/c" + NUL = 31, padded to 32: one spare byte.
        let mut data = thin(Endian::Little, true, &[Cmd::Load("/a/b/c")]);

        let rewrite = rewrite(&mut data, |_, _| Some("/a/b/cd".into())).unwrap();

        assert!(!rewrite.written.is_empty());
        assert!(rewrite.unfit.is_empty());
        assert_eq!(
            values(&inspect(&data).unwrap()),
            [(PathKind::Dylib, "/a/b/cd")]
        );
    }

    #[test]
    fn a_replacement_that_does_not_fit_is_reported_and_nothing_changes() {
        let old = "@@HOMEBREW_CELLAR@@/x";
        let mut data = thin(
            Endian::Little,
            true,
            &[
                Cmd::Padded(PathKind::Rpath, old, 0),
                Cmd::Load("/usr/lib/libz.1.dylib"),
            ],
        );
        let original = data.clone();

        let rewrite = rewrite(&mut data, |_, path| {
            Some(path.replace("@@HOMEBREW_CELLAR@@", "/opt/nextbrew/Cellar"))
        })
        .unwrap();

        assert!(rewrite.written.is_empty());
        assert!(!rewrite.signed);
        assert_eq!(
            rewrite.unfit,
            [UnfitChange {
                kind: PathKind::Rpath,
                old: old.into(),
                new: "/opt/nextbrew/Cellar/x".into(),
            }]
        );
        assert_eq!(data, original);
    }

    /// Insert `pad` zero bytes between a thin file's load commands and the
    /// code after them, as a linker leaves header padding.
    fn with_header_padding(mut data: Vec<u8>, pad: usize) -> Vec<u8> {
        let header = thin_header(&data, 0).unwrap();
        let sizeofcmds = read_u32(&data, 20, header.endian).unwrap() as usize;
        let at = header.size + sizeofcmds;
        data.splice(at..at, std::iter::repeat_n(0u8, pad));
        data
    }

    #[test]
    fn a_replacement_that_does_not_fit_grows_into_the_header_padding() {
        // icu4c@78's makeconv: the Cellar path is a byte longer than its slot.
        let old = "@@HOMEBREW_CELLAR@@/icu4c@78/78.3/lib/libicutu.78.dylib";
        let new = "/opt/nextbrew/Cellar/icu4c@78/78.3/lib/libicutu.78.dylib";
        let slice = |endian, is64| {
            with_header_padding(
                thin(
                    endian,
                    is64,
                    &[
                        Cmd::Padded(PathKind::Dylib, old, 0),
                        Cmd::Load("/usr/lib/libSystem.B.dylib"),
                        Cmd::Signature,
                    ],
                ),
                64,
            )
        };
        let original = fat(&[slice(Endian::Little, true), slice(Endian::Big, false)]);
        let mut data = original.clone();

        let rewrite = rewrite(&mut data, |_, path| {
            Some(path.replace("@@HOMEBREW_CELLAR@@", "/opt/nextbrew/Cellar"))
        })
        .unwrap();

        assert!(rewrite.unfit.is_empty(), "{:?}", rewrite.unfit);
        assert_eq!(data.len(), original.len());
        let info = inspect(&data).unwrap();
        let dylibs = [
            (PathKind::Dylib, new),
            (PathKind::Dylib, "/usr/lib/libSystem.B.dylib"),
        ];
        assert_eq!(values(&info), [dylibs, dylibs].concat());
        assert!(info.signed, "the commands after it moved, not lost");
        // Writing back only the reported ranges gives the same file.
        let mut patched = original.clone();
        for range in &rewrite.written {
            patched[range.clone()].copy_from_slice(&data[range.clone()]);
        }
        assert_eq!(patched, data);
    }

    #[test]
    fn padding_need_not_be_zero_to_grow_into() {
        let old = "@@HOMEBREW_CELLAR@@/x/1.0/lib/libx.dylib";
        let mut data = with_header_padding(
            thin(
                Endian::Little,
                true,
                &[Cmd::Padded(PathKind::Dylib, old, 0)],
            ),
            64,
        );
        let header = thin_header(&data, 0).unwrap();
        let end = header.size + read_u32(&data, 20, header.endian).unwrap() as usize;
        data[end..end + 64].fill(0xaa);

        let rewrite = rewrite(&mut data, |_, path| {
            Some(path.replace("@@HOMEBREW_CELLAR@@", "/opt/nextbrew/Cellar"))
        })
        .unwrap();

        assert!(rewrite.unfit.is_empty(), "{:?}", rewrite.unfit);
        assert_eq!(
            values(&inspect(&data).unwrap()),
            [(PathKind::Dylib, "/opt/nextbrew/Cellar/x/1.0/lib/libx.dylib")]
        );
    }

    #[test]
    fn unchanged_paths_are_not_written() {
        let mut data = thin(Endian::Little, true, &[Cmd::Load("/usr/lib/libz.1.dylib")]);
        let original = data.clone();

        let rewrite = rewrite(&mut data, |_, path| Some(path.to_string())).unwrap();

        assert!(rewrite.written.is_empty());
        assert_eq!(data, original);
    }

    #[test]
    fn rejects_files_that_are_not_macho() {
        assert_eq!(inspect(b"").unwrap_err(), MachoError::NotMacho);
        assert_eq!(inspect(b"#!/bin/sh\n").unwrap_err(), MachoError::NotMacho);
        assert_eq!(inspect(b"!<arch>\n").unwrap_err(), MachoError::NotMacho);
        // A Java class file: fat magic followed by a version, not an arch count.
        let class = [0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x41, 0, 0, 0, 0];
        assert_eq!(inspect(&class).unwrap_err(), MachoError::NotMacho);
        assert!(
            has_macho_magic(&class),
            "the magic alone cannot tell them apart"
        );
    }

    #[test]
    fn rejects_truncated_and_malformed_files() {
        let good = thin(Endian::Little, true, &[Cmd::Load("/usr/lib/libz.1.dylib")]);

        // Cut inside the load commands.
        let truncated = &good[..40];
        assert_eq!(
            inspect(truncated).unwrap_err(),
            MachoError::Truncated("load commands")
        );

        // A command claiming to be smaller than its fixed header.
        let mut tiny = good.clone();
        tiny[32 + 4..32 + 8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            inspect(&tiny).unwrap_err(),
            MachoError::BadLoadCommand {
                index: 0,
                cmd: macho::LC_LOAD_DYLIB
            }
        );

        // A string offset outside the command.
        let mut escaped = good.clone();
        escaped[32 + 8..32 + 12].copy_from_slice(&200u32.to_le_bytes());
        assert_eq!(
            inspect(&escaped).unwrap_err(),
            MachoError::BadLoadCommand {
                index: 0,
                cmd: macho::LC_LOAD_DYLIB
            }
        );

        // A path with no terminator.
        let mut unterminated = thin(
            Endian::Little,
            true,
            &[Cmd::Padded(PathKind::Dylib, "/a/b/c", 0)],
        );
        let end = unterminated.len() - 1; // the trailing "code" byte
        unterminated[end - 1] = b'x';
        assert_eq!(
            inspect(&unterminated).unwrap_err(),
            MachoError::UnterminatedString {
                index: 0,
                cmd: macho::LC_LOAD_DYLIB
            }
        );

        // A fat arch that points past the end of the file.
        let mut bad_fat = fat(std::slice::from_ref(&good));
        bad_fat[8 + 12..8 + 16].copy_from_slice(&0xffffu32.to_be_bytes());
        assert_eq!(
            inspect(&bad_fat).unwrap_err(),
            MachoError::Truncated("fat arch")
        );
    }

    /// A real signed system binary: what we read must match `otool -L`.
    #[test]
    #[cfg(target_os = "macos")]
    fn matches_otool_on_a_system_binary() {
        use std::process::Command;

        let data = std::fs::read("/bin/ls").unwrap();
        let info = inspect(&data).unwrap();
        assert!(info.signed, "/bin/ls is signed by Apple");

        let output = Command::new("otool")
            .args(["-L", "/bin/ls"])
            .output()
            .unwrap();
        let otool: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with('\t'))
            .map(|line| {
                line.trim()
                    .split(" (compatibility")
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(!otool.is_empty());

        let ours: Vec<&str> = info
            .paths
            .iter()
            .filter(|p| p.kind == PathKind::Dylib)
            .map(|p| p.value.as_str())
            .collect();
        // `/bin/ls` is fat; otool prints every architecture's list, so ours
        // is otool's list repeated per slice.
        assert_eq!(ours.len() % otool.len(), 0);
        for (i, path) in ours.iter().enumerate() {
            assert_eq!(*path, otool[i % otool.len()]);
        }
    }

    #[test]
    fn has_macho_magic_recognises_thin_and_fat_files() {
        assert!(has_macho_magic(b"\xcf\xfa\xed\xfe"));
        assert!(has_macho_magic(b"\xfe\xed\xfa\xce"));
        assert!(has_macho_magic(b"\xca\xfe\xba\xbe"));
        assert!(has_macho_magic(b"\xca\xfe\xba\xbe\0\0\0\x02"));
        assert!(!has_macho_magic(b"\xca\xfe\xba\xbe\0\0\0\x34"));
        assert!(!has_macho_magic(b"\x7fELF"));
        assert!(!has_macho_magic(b"\xcf\xfa"));
    }

    /// A 64-bit file of `pages` 4K pages, then an ad-hoc signature whose
    /// SHA-256 code directory hashes them.
    fn adhoc_signed(pages: usize, flags: u32, cms: usize) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let code_limit = pages * 4096;
        let mut out = vec![0u8; code_limit];
        let le =
            |out: &mut [u8], at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_le_bytes());
        le(&mut out, 0, macho::MH_MAGIC_64);
        le(&mut out, 16, 1);
        le(&mut out, 20, 16);
        le(&mut out, 32, macho::LC_CODE_SIGNATURE);
        le(&mut out, 36, 16);
        le(&mut out, 40, code_limit as u32);
        for (i, b) in out[48..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let mut cd = Vec::new();
        for v in [
            CSMAGIC_CODEDIRECTORY,
            0,
            0x20400,
            flags,
            88,
            0,
            0,
            pages as u32,
            code_limit as u32,
        ] {
            cd.extend_from_slice(&v.to_be_bytes());
        }
        cd.extend_from_slice(&[32, CS_HASHTYPE_SHA256, 0, 12]);
        cd.resize(88, 0);
        cd.resize(88 + 32 * pages, 0);
        let cd_len = cd.len() as u32;
        cd[4..8].copy_from_slice(&cd_len.to_be_bytes());

        let mut sig = Vec::new();
        let header = 12 + 2 * 8;
        let blob = [0xfade_0b01u32.to_be_bytes(), (8 + cms as u32).to_be_bytes()].concat();
        for v in [
            CSMAGIC_EMBEDDED_SIGNATURE,
            (header + cd.len() + 8 + cms) as u32,
            2,
            CSSLOT_CODEDIRECTORY,
            header as u32,
            CSSLOT_SIGNATURESLOT,
            (header + cd.len()) as u32,
        ] {
            sig.extend_from_slice(&v.to_be_bytes());
        }
        sig.extend_from_slice(&cd);
        sig.extend_from_slice(&blob);
        sig.resize(sig.len() + cms, 0);
        le(&mut out, 44, sig.len() as u32);
        out.extend_from_slice(&sig);
        for i in 0..pages {
            let digest = Sha256::digest(&out[i * 4096..(i + 1) * 4096]);
            let at = code_limit + header + 88 + 32 * i;
            out[at..at + 32].copy_from_slice(&digest);
        }
        out
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn rehashes_the_pages_an_adhoc_signature_covers() {
        let original = adhoc_signed(3, CS_ADHOC, 0);
        assert_eq!(rehash_adhoc(&mut original.clone(), &[]), Some(vec![]));

        let mut patched = original.clone();
        patched[5000] ^= 1;
        let written = rehash_adhoc(&mut patched, &[5000..5001]).unwrap();
        let hash_at = 3 * 4096 + 28 + 88 + 32;
        assert_eq!(written, vec![hash_at..hash_at + 32]);

        use sha2::{Digest, Sha256};
        assert_eq!(
            patched[hash_at..hash_at + 32],
            Sha256::digest(&patched[4096..8192])[..]
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn leaves_other_signatures_to_codesign() {
        assert_eq!(
            rehash_adhoc(&mut adhoc_signed(2, 0, 0), &[]),
            None,
            "not ad-hoc"
        );
        assert_eq!(
            rehash_adhoc(&mut adhoc_signed(2, CS_ADHOC, 64), &[]),
            None,
            "has a CMS signature"
        );
        let mut data = adhoc_signed(2, CS_ADHOC, 0);
        let sig = 2 * 4096;
        assert_eq!(
            rehash_adhoc(&mut data, &[sig + 40..sig + 41]),
            None,
            "patched the signature"
        );
        assert_eq!(
            rehash_adhoc(&mut thin(Endian::Little, true, &[Cmd::Other]), &[]),
            None,
            "unsigned"
        );
    }
}
