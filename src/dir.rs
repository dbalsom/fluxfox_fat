use std::ascii::AsciiExt;
use std::fmt;
use std::io::prelude::*;
use std::io;
use std::io::{Cursor, ErrorKind, SeekFrom};
use std::cmp;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

#[cfg(feature = "chrono")]
use chrono::{TimeZone, Local};
#[cfg(feature = "chrono")]
use chrono;

use fs::{FileSystemRef, DiskSlice};
use file::File;

#[derive(Clone)]
pub(crate) enum DirRawStream<'a, 'b: 'a> {
    File(File<'a, 'b>),
    Root(DiskSlice<'a, 'b>),
}

impl <'a, 'b> DirRawStream<'a, 'b> {
    pub(crate) fn abs_pos(&self) -> Option<u64> {
        match self {
            &DirRawStream::File(ref file) => file.abs_pos(),
            &DirRawStream::Root(ref slice) => Some(slice.abs_pos()),
        }
    }
}

impl <'a, 'b> Read for DirRawStream<'a, 'b> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            &mut DirRawStream::File(ref mut file) => file.read(buf),
            &mut DirRawStream::Root(ref mut raw) => raw.read(buf),
        }
    }
}

impl <'a, 'b> Write for DirRawStream<'a, 'b> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            &mut DirRawStream::File(ref mut file) => file.write(buf),
            &mut DirRawStream::Root(ref mut raw) => raw.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            &mut DirRawStream::File(ref mut file) => file.flush(),
            &mut DirRawStream::Root(ref mut raw) => raw.flush(),
        }
    }
}

impl <'a, 'b> Seek for DirRawStream<'a, 'b> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            &mut DirRawStream::File(ref mut file) => file.seek(pos),
            &mut DirRawStream::Root(ref mut raw) => raw.seek(pos),
        }
    }
}

bitflags! {
    /// FAT file attributes
    #[derive(Default)]
    pub struct FileAttributes: u8 {
        const READ_ONLY  = 0x01;
        const HIDDEN     = 0x02;
        const SYSTEM     = 0x04;
        const VOLUME_ID  = 0x08;
        const DIRECTORY  = 0x10;
        const ARCHIVE    = 0x20;
        const LFN        = Self::READ_ONLY.bits | Self::HIDDEN.bits
                         | Self::SYSTEM.bits | Self::VOLUME_ID.bits;
    }
}

const LFN_PART_LEN: usize = 13;
const DIR_ENTRY_SIZE: u64 = 32;
const DIR_ENTRY_FREE_FLAG: u8 = 0xE5;
const LFN_ENTRY_LAST_FLAG: u8 = 0x40;

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub(crate) struct DirFileEntryData {
    name: [u8; 11],
    attrs: FileAttributes,
    reserved_0: u8,
    create_time_0: u8,
    create_time_1: u16,
    create_date: u16,
    access_date: u16,
    first_cluster_hi: u16,
    modify_time: u16,
    modify_date: u16,
    first_cluster_lo: u16,
    size: u32,
}

impl DirFileEntryData {
    pub(crate) fn first_cluster(&self) -> Option<u32> {
        let n = ((self.first_cluster_hi as u32) << 16) | self.first_cluster_lo as u32;
        if n == 0 { None } else { Some(n) }
    }
    
    pub(crate) fn set_first_cluster(&mut self, cluster: Option<u32>) {
        let n = cluster.unwrap_or(0);
        self.first_cluster_hi = (n >> 16) as u16;
        self.first_cluster_lo = (n & 0xFFFF) as u16;
    }
    
    pub(crate) fn size(&self) -> Option<u32> {
        if self.is_file() {
            Some(self.size)
        } else {
            None
        }
    }
    
    pub(crate) fn set_size(&mut self, size: u32) {
        self.size = size;
    }
    
    pub fn is_dir(&self) -> bool {
        self.attrs.contains(FileAttributes::DIRECTORY)
    }
    
    pub fn is_file(&self) -> bool {
        !self.is_dir()
    }
    
    pub(crate) fn set_modified(&mut self, date_time: DateTime) {
        self.modify_date = date_time.date.to_u16();
        self.modify_time = date_time.time.to_u16();
    }
    
    pub(crate) fn serialize(&self, wrt: &mut Write) -> io::Result<()> {
        wrt.write_all(&self.name)?;
        wrt.write_u8(self.attrs.bits())?;
        wrt.write_u8(self.reserved_0)?;
        wrt.write_u8(self.create_time_0)?;
        wrt.write_u16::<LittleEndian>(self.create_time_1)?;
        wrt.write_u16::<LittleEndian>(self.create_date)?;
        wrt.write_u16::<LittleEndian>(self.access_date)?;
        wrt.write_u16::<LittleEndian>(self.first_cluster_hi)?;
        wrt.write_u16::<LittleEndian>(self.modify_time)?;
        wrt.write_u16::<LittleEndian>(self.modify_date)?;
        wrt.write_u16::<LittleEndian>(self.first_cluster_lo)?;
        wrt.write_u32::<LittleEndian>(self.size)?;
        Ok(())
    }
    
    fn is_free(&self) -> bool {
        self.name[0] == DIR_ENTRY_FREE_FLAG
    }
    
    fn is_end(&self) -> bool {
        self.name[0] == 0
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
struct DirLfnEntryData {
    order: u8,
    name_0: [u16; 5],
    attrs: FileAttributes,
    entry_type: u8,
    checksum: u8,
    name_1: [u16; 6],
    reserved_0: u16,
    name_2: [u16; 2],
}

impl DirLfnEntryData {
    fn serialize(&self, wrt: &mut Write) -> io::Result<()> {
        wrt.write_u8(self.order)?;
        for ch in self.name_0.iter() {
            wrt.write_u16::<LittleEndian>(*ch)?;
        }
        wrt.write_u8(self.attrs.bits())?;
        wrt.write_u8(self.entry_type)?;
        wrt.write_u8(self.checksum)?;
        for ch in self.name_1.iter() {
            wrt.write_u16::<LittleEndian>(*ch)?;
        }
        wrt.write_u16::<LittleEndian>(self.reserved_0)?;
        for ch in self.name_2.iter() {
            wrt.write_u16::<LittleEndian>(*ch)?;
        }
        Ok(())
    }
    
    fn is_free(&self) -> bool {
        self.order == DIR_ENTRY_FREE_FLAG
    }
    
    fn is_end(&self) -> bool {
        self.order == 0
    }
}

#[derive(Clone, Debug)]
enum DirEntryData {
    File(DirFileEntryData),
    Lfn(DirLfnEntryData),
}

impl DirEntryData {
    fn serialize(&mut self, wrt: &mut Write) -> io::Result<()> {
        match self {
            &mut DirEntryData::File(ref mut file) => file.serialize(wrt),
            &mut DirEntryData::Lfn(ref mut lfn) => lfn.serialize(wrt),
        }
    }
    
    fn deserialize(rdr: &mut Read) -> io::Result<DirEntryData> {
        let mut name = [0; 11];
        rdr.read_exact(&mut name)?;
        let attrs = FileAttributes::from_bits_truncate(rdr.read_u8()?);
        if attrs & FileAttributes::LFN == FileAttributes::LFN {
            let mut data = DirLfnEntryData {
                attrs, ..Default::default()
            };
            let mut cur = Cursor::new(&name);
            data.order = cur.read_u8()?;
            cur.read_u16_into::<LittleEndian>(&mut data.name_0)?;
            data.entry_type = rdr.read_u8()?;
            data.checksum = rdr.read_u8()?;
            rdr.read_u16_into::<LittleEndian>(&mut data.name_1)?;
            data.reserved_0 = rdr.read_u16::<LittleEndian>()?;
            rdr.read_u16_into::<LittleEndian>(&mut data.name_2)?;
            Ok(DirEntryData::Lfn(data))
        } else {
            let data = DirFileEntryData {
                name,
                attrs,
                reserved_0:       rdr.read_u8()?,
                create_time_0:    rdr.read_u8()?,
                create_time_1:    rdr.read_u16::<LittleEndian>()?,
                create_date:      rdr.read_u16::<LittleEndian>()?,
                access_date:      rdr.read_u16::<LittleEndian>()?,
                first_cluster_hi: rdr.read_u16::<LittleEndian>()?,
                modify_time:      rdr.read_u16::<LittleEndian>()?,
                modify_date:      rdr.read_u16::<LittleEndian>()?,
                first_cluster_lo: rdr.read_u16::<LittleEndian>()?,
                size:             rdr.read_u32::<LittleEndian>()?,
            };
            Ok(DirEntryData::File(data))
        }
    }
    
    fn is_free(&self) -> bool {
        match self {
            &DirEntryData::File(ref file) => file.is_free(),
            &DirEntryData::Lfn(ref lfn) => lfn.is_free(),
        }
    }
    
    fn is_end(&self) -> bool {
        match self {
            &DirEntryData::File(ref file) => file.is_end(),
            &DirEntryData::Lfn(ref lfn) => lfn.is_end(),
        }
    }
}

/// DOS compatible date
#[derive(Clone, Copy, Debug)]
pub struct Date {
    pub year: u16,
    pub month: u16,
    pub day: u16,
}

impl Date {
    pub(crate) fn from_u16(dos_date: u16) -> Self {
        let (year, month, day) = ((dos_date >> 9) + 1980, (dos_date >> 5) & 0xF, dos_date & 0x1F);
        Date { year, month, day }
    }
    
    fn to_u16(&self) -> u16 {
        ((self.year - 1980) << 9) | (self.month << 5) | self.day
    }
}

/// DOS compatible time
#[derive(Clone, Copy, Debug)]
pub struct Time {
    pub hour: u16,
    pub min: u16,
    pub sec: u16,
}

impl Time {
    pub(crate) fn from_u16(dos_time: u16) -> Self {
        let (hour, min, sec) = (dos_time >> 11, (dos_time >> 5) & 0x3F, (dos_time & 0x1F) * 2);
        Time { hour, min, sec }
    }
    
    fn to_u16(&self) -> u16 {
        (self.hour << 11) | (self.min << 5) | (self.sec / 2)
    }
}

/// DOS compatible date and time
#[derive(Clone, Copy, Debug)]
pub struct DateTime {
    pub date: Date,
    pub time: Time,
}

impl DateTime {
    pub(crate) fn from_u16(dos_date: u16, dos_time: u16) -> Self {
        DateTime {
            date: Date::from_u16(dos_date),
            time: Time::from_u16(dos_time),
        }
    }
}

#[cfg(feature = "chrono")]
impl From<Date> for chrono::Date<Local> {
    fn from(date: Date) -> Self {
        Local.ymd(date.year as i32, date.month as u32, date.day as u32)
    }
}

#[cfg(feature = "chrono")]
impl From<DateTime> for chrono::DateTime<Local> {
    fn from(date_time: DateTime) -> Self {
        chrono::Date::<Local>::from(date_time.date)
            .and_hms(date_time.time.hour as u32, date_time.time.min as u32, date_time.time.sec as u32)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FileEntryInfo {
    pub(crate) data: DirFileEntryData,
    pos: u64,
}

impl FileEntryInfo {
    pub(crate) fn write(&self, fs: FileSystemRef) -> io::Result<()> {
        let mut disk = fs.disk.borrow_mut();
        disk.seek(io::SeekFrom::Start(self.pos))?;
        self.data.serialize(&mut *disk)
    }
}

/// FAT directory entry.
///
/// Returned by DirIter.
#[derive(Clone)]
pub struct DirEntry<'a, 'b: 'a> {
    data: DirFileEntryData,
    lfn: Vec<u16>,
    entry_pos: u64,
    offset_range: (u64, u64),
    fs: FileSystemRef<'a, 'b>,
}

impl <'a, 'b> DirEntry<'a, 'b> {
    /// Returns short file name
    pub fn short_file_name(&self) -> String {
        let name_str = String::from_utf8_lossy(&self.data.name[0..8]);
        let ext_str = String::from_utf8_lossy(&self.data.name[8..11]);
        let name_trimmed = name_str.trim_right();
        let ext_trimmed = ext_str.trim_right();
        if ext_trimmed.is_empty() {
            name_trimmed.to_string()
        } else {
            format!("{}.{}", name_trimmed, ext_trimmed)
        }
    }
    
    /// Returns long file name or if it doesn't exist fallbacks to short file name.
    pub fn file_name(&self) -> String {
        if self.lfn.len() > 0 {
            String::from_utf16_lossy(&self.lfn)
        } else {
            self.short_file_name()
        }
    }
    
    /// Returns file attributes
    pub fn attributes(&self) -> FileAttributes {
        self.data.attrs
    }
    
    /// Checks if entry belongs to directory.
    pub fn is_dir(&self) -> bool {
        self.data.is_dir()
    }
    
    /// Checks if entry belongs to regular file.
    pub fn is_file(&self) -> bool {
        self.data.is_file()
    }
    
    pub(crate) fn first_cluster(&self) -> Option<u32> {
        self.data.first_cluster()
    }
    
    fn entry_info(&self) -> FileEntryInfo {
        FileEntryInfo {
            data: self.data.clone(),
            pos: self.entry_pos,
        }
    }
    
    /// Returns File struct for this entry.
    ///
    /// Panics if this is not a file.
    pub fn to_file(&self) -> File<'a, 'b> {
        assert!(!self.is_dir(), "Not a file entry");
        File::new(self.first_cluster(), Some(self.entry_info()), self.fs)
    }
    
    /// Returns Dir struct for this entry.
    ///
    /// Panics if this is not a directory.
    pub fn to_dir(&self) -> Dir<'a, 'b> {
        assert!(self.is_dir(), "Not a directory entry");
        match self.first_cluster() {
            Some(n) => {
                let file = File::new(Some(n), Some(self.entry_info()), self.fs);
                Dir::new(DirRawStream::File(file), self.fs)
            },
            None => self.fs.root_dir(),
        }
    }
    
    /// Returns file size or 0 for directory.
    pub fn len(&self) -> u64 {
        self.data.size as u64
    }
    
    /// Returns file creation date and time.
    pub fn created(&self) -> DateTime {
        DateTime::from_u16(self.data.create_date, self.data.create_time_1)
    }
    
    /// Returns file last access date.
    pub fn accessed(&self) -> Date {
        Date::from_u16(self.data.access_date)
    }
    
    /// Returns file last modification date and time.
    pub fn modified(&self) -> DateTime {
        DateTime::from_u16(self.data.modify_date, self.data.modify_time)
    }
}

impl <'a, 'b> fmt::Debug for DirEntry<'a, 'b> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.data.fmt(f)
    }
}

/// FAT directory
#[derive(Clone)]
pub struct Dir<'a, 'b: 'a> {
    stream: DirRawStream<'a, 'b>,
    fs: FileSystemRef<'a, 'b>,
}

impl <'a, 'b> Dir<'a, 'b> {
    
    pub(crate) fn new(stream: DirRawStream<'a, 'b>, fs: FileSystemRef<'a, 'b>) -> Dir<'a, 'b> {
        Dir { stream, fs }
    }
    
    /// Creates directory entries iterator
    pub fn iter(&self) -> DirIter<'a, 'b> {
        DirIter {
            stream: self.stream.clone(),
            fs: self.fs.clone(),
            err: false,
        }
    }
    
    fn split_path<'c>(path: &'c str) -> (&'c str, Option<&'c str>) {
        let mut path_split = path.trim_matches('/').splitn(2, "/");
        let comp = path_split.next().unwrap(); // safe unwrap - splitn always returns at least one element
        let rest_opt = path_split.next();
        (comp, rest_opt)
    }
    
    fn find_entry(&mut self, name: &str) -> io::Result<DirEntry<'a, 'b>> {
        for r in self.iter() {
            let e = r?;
            if e.file_name().eq_ignore_ascii_case(name) {
                return Ok(e);
            }
        }
        Err(io::Error::new(ErrorKind::NotFound, "file not found"))
    }
    
    /// Opens existing directory
    pub fn open_dir(&mut self, path: &str) -> io::Result<Dir<'a, 'b>> {
        let (name, rest_opt) = Self::split_path(path);
        let e = self.find_entry(name)?;
        match rest_opt {
            Some(rest) => e.to_dir().open_dir(rest),
            None => Ok(e.to_dir())
        }
    }
    
    /// Opens existing file.
    pub fn open_file(&mut self, path: &str) -> io::Result<File<'a, 'b>> {
        let (name, rest_opt) = Self::split_path(path);
        let e = self.find_entry(name)?;
        match rest_opt {
            Some(rest) => e.to_dir().open_file(rest),
            None => Ok(e.to_file())
        }
    }
    
    /// Creates new file or opens existing.
    pub fn create_file(&mut self, path: &str) -> io::Result<File<'a, 'b>> {
        let (name, rest_opt) = Self::split_path(path);
        let r = self.find_entry(name);
        match rest_opt {
            Some(rest) => r?.to_dir().create_file(rest),
            None => {
                match r {
                    Err(_) => Ok(self.create_file_entry(name)?.to_file()),
                    Ok(e) => Ok(e.to_file())
                }
            }
        }
    }
    
    fn is_empty(&mut self) -> io::Result<bool> {
        for r in self.iter() {
            let e = r?;
            let name = e.file_name();
            if name != "." && name != ".." {
                return Ok(false);
            }
        }
        Ok(true)
    }
    
    /// Removes existing file or directory.
    ///
    /// Make sure there is no reference to this file (no File instance) or filesystem corruption
    /// can happen.
    pub fn remove(&mut self, path: &str) -> io::Result<()> {
        let (name, rest_opt) = Self::split_path(path);
        let e = self.find_entry(name)?;
        match rest_opt {
            Some(rest) => e.to_dir().remove(rest),
            None => {
                trace!("removing {}", path);
                if e.is_dir() && !e.to_dir().is_empty()? {
                    return Err(io::Error::new(ErrorKind::NotFound, "removing non-empty directory is denied"));
                }
                match e.first_cluster() {
                    Some(n) => self.fs.cluster_iter(n).free()?,
                    _ => {},
                }
                let mut stream = self.stream.clone();
                stream.seek(SeekFrom::Start(e.offset_range.0 as u64))?;
                let num = (e.offset_range.1 - e.offset_range.0) as usize / DIR_ENTRY_SIZE as usize;
                for _ in 0..num {
                    let mut data = DirEntryData::deserialize(&mut stream)?;
                    trace!("removing dir entry {:?}", data);
                    match data {
                        DirEntryData::File(ref mut data) =>
                            data.name[0] = DIR_ENTRY_FREE_FLAG,
                        DirEntryData::Lfn(ref mut data) => data.order = DIR_ENTRY_FREE_FLAG,
                    };
                    stream.seek(SeekFrom::Current(-(DIR_ENTRY_SIZE as i64)))?;
                    data.serialize(&mut stream)?;
                }
                Ok(())
            }
        }
    }
    
    fn find_free_entries(&mut self, num_entries: usize) -> io::Result<DirRawStream<'a, 'b>> {
        let mut stream = self.stream.clone();
        let mut first_free = 0;
        let mut num_free = 0;
        let mut i = 0;
        loop {
            let data = DirEntryData::deserialize(&mut stream)?;
            if data.is_free() {
                if num_free == 0 {
                    first_free = i;
                }
                num_free += 1;
                if num_free == num_entries {
                    stream.seek(io::SeekFrom::Start(first_free as u64 * DIR_ENTRY_SIZE))?;
                    return Ok(stream);
                }
            } else if data.is_end() {
                if num_free == 0 {
                    first_free = i;
                }
                stream.seek(io::SeekFrom::Start(first_free as u64 * DIR_ENTRY_SIZE))?;
                // FIXME: make sure new allocated cluster is zeroed
                return Ok(stream);
            } else {
                num_free = 0;
            }
            i += 1;
        }
    }
    
    fn gen_short_name(name: &str) -> [u8;11] {
        // short name is always uppercase
        let mut name_upper = name.to_uppercase();
        // padded by ' '
        let mut short_name = [0x20u8; 11];
        // find extension after last dot
        match name_upper.rfind('.') {
            Some(index) => {
                // copy first 3 characters of extension
                let short_ext_len = cmp::min(name_upper.len() - index - 1, 3);
                short_name[8..8+short_ext_len].copy_from_slice(name_upper[index..index+short_ext_len].as_bytes());
                // remove extension with dot from name_upper
                name_upper.truncate(index);
            },
            None => {},
        }
        // copy first 8 characters of name
        let short_name_len = cmp::min(name_upper.len(), 8);
        short_name[..short_name_len].copy_from_slice(name_upper[..short_name_len].as_bytes());
        // FIXME: make sure short name is unique...
        short_name
    }
    
    fn create_file_entry(&mut self, name: &str) -> io::Result<DirEntry<'a, 'b>> {
        if name.len() > 255 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "filename too long"));
        }
        let num_lfn_entries = (name.len() + LFN_PART_LEN - 1) / LFN_PART_LEN;
        let num_entries = num_lfn_entries + 1; // multiple lfn entries + one file entry
        let mut stream = self.find_free_entries(num_entries)?;
        let start_pos = stream.seek(io::SeekFrom::Current(0))?;
        let short_name = Self::gen_short_name(name);
        let lfn_chsum = lfn_checksum(&short_name);
        let lfn_utf8 = name.encode_utf16().collect::<Vec<u16>>();
        for i in 0..num_lfn_entries {
            let lfn_index = num_lfn_entries - i;
            let mut order = lfn_index as u8;
            if i == 0 {
                order |= LFN_ENTRY_LAST_FLAG;
            }
            debug_assert!(order > 0);
            let lfn_pos = (lfn_index - 1) * LFN_PART_LEN;
            let mut lfn_part = [0xFFFFu16; LFN_PART_LEN];
            let lfn_part_len = cmp::min(name.len() - lfn_pos, LFN_PART_LEN);
            lfn_part[..lfn_part_len].copy_from_slice(&lfn_utf8[lfn_pos..lfn_pos+lfn_part_len]);
            if lfn_part_len < LFN_PART_LEN {
                lfn_part[lfn_part_len] = 0;
            }
            let mut lfn_entry = DirLfnEntryData {
                order,
                attrs: FileAttributes::LFN,
                checksum: lfn_chsum,
                ..Default::default()
            };
            lfn_entry.name_0.copy_from_slice(&lfn_part[0..5]);
            lfn_entry.name_1.copy_from_slice(&lfn_part[5..5+6]);
            lfn_entry.name_2.copy_from_slice(&lfn_part[11..11+2]);
            lfn_entry.serialize(&mut stream)?;
        }
        let raw_entry = DirFileEntryData {
            name: short_name,
            ..Default::default()
        };
        raw_entry.serialize(&mut stream)?;
        let end_pos = stream.seek(io::SeekFrom::Current(0))?;
        let abs_pos = stream.abs_pos().map(|p| p - DIR_ENTRY_SIZE);
        return Ok(DirEntry {
            data: raw_entry,
            lfn: Vec::new(),
            fs: self.fs,
            entry_pos: abs_pos.unwrap(), // safe
            offset_range: (start_pos, end_pos),
        });
    }
}

/// Directory entries iterator.
#[derive(Clone)]
pub struct DirIter<'a, 'b: 'a> {
    stream: DirRawStream<'a, 'b>,
    fs: FileSystemRef<'a, 'b>,
    err: bool,
}

impl <'a, 'b> DirIter<'a, 'b> {
    fn read_dir_entry_raw_data(&mut self) -> io::Result<DirEntryData> {
        DirEntryData::deserialize(&mut self.stream)
    }
    
    fn read_dir_entry(&mut self) -> io::Result<Option<DirEntry<'a, 'b>>> {
        let mut lfn_buf = LongNameBuilder::new();
        let mut offset = self.stream.seek(SeekFrom::Current(0))?;
        let mut begin_offset = offset;
        loop {
            let raw_entry = self.read_dir_entry_raw_data()?;
            offset += DIR_ENTRY_SIZE;
            match raw_entry {
                DirEntryData::File(data) => {
                    // Check if this is end of dif
                    if data.is_end() {
                        return Ok(None);
                    }
                    // Check if this is deleted or volume ID entry
                    if data.is_free() || data.attrs.contains(FileAttributes::VOLUME_ID) {
                        lfn_buf.clear();
                        begin_offset = offset;
                        continue;
                    }
                    // Get entry position on volume
                    let entry_pos = self.stream.abs_pos().map(|p| p - DIR_ENTRY_SIZE);
                    // Check if LFN checksum is valid
                    lfn_buf.validate_chksum(&data.name);
                    return Ok(Some(DirEntry {
                        data,
                        lfn: lfn_buf.to_vec(),
                        fs: self.fs,
                        entry_pos: entry_pos.unwrap(), // safe
                        offset_range: (begin_offset, offset),
                    }));
                },
                DirEntryData::Lfn(data) => {
                    // Check if this is deleted entry
                    if data.is_free() {
                        lfn_buf.clear();
                        begin_offset = offset;
                        continue;
                    }
                    // Append to LFN buffer
                    lfn_buf.process(&data);
                }
            }
        }
    }
}

impl <'a, 'b> Iterator for DirIter<'a, 'b> {
    type Item = io::Result<DirEntry<'a, 'b>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.err {
            return None;
        }
        let r = self.read_dir_entry();
        match r {
            Ok(Some(e)) => Some(Ok(e)),
            Ok(None) => None,
            Err(err) => {
                self.err = true;
                Some(Err(err))
            },
        }
    }
}

struct LongNameBuilder {
    buf: Vec<u16>,
    chksum: u8,
    index: u8,
}

fn lfn_checksum(short_name: &[u8]) -> u8 {
    let mut chksum = 0u8;
    for i in 0..11 {
        chksum = (((chksum & 1) << 7) as u16 + (chksum >> 1) as u16 + short_name[i] as u16) as u8;
    }
    chksum
}

impl LongNameBuilder {
    fn new() -> LongNameBuilder {
        LongNameBuilder {
            buf: Vec::<u16>::new(),
            chksum: 0,
            index: 0,
        }
    }
    
    fn clear(&mut self) {
        self.buf.clear();
        self.index = 0;
    }
    
    fn to_vec(mut self) -> Vec<u16> {
        if self.index == 1 {
            self.truncate();
            self.buf
        } else {
            warn!("unfinished LFN sequence {}", self.index);
            Vec::<u16>::new()
        }
    }
    
    fn truncate(&mut self) {
        // Truncate 0 and 0xFFFF characters from LFN buffer
        let mut lfn_len = self.buf.len();
        loop {
            if lfn_len == 0 {
                break;
            }
            match self.buf[lfn_len-1] {
                0xFFFF | 0 => lfn_len -= 1,
                _ => break,
            }
        }
        self.buf.truncate(lfn_len);
    }
    
    fn process(&mut self, data: &DirLfnEntryData) {
        let is_last = (data.order & LFN_ENTRY_LAST_FLAG) != 0;
        let index = data.order & 0x1F;
        if index == 0 {
            // Corrupted entry
            warn!("currupted lfn entry! {:x}", data.order);
            self.clear();
            return;
        }
        if is_last {
            // last entry is actually first entry in stream
            self.index = index;
            self.chksum = data.checksum;
            self.buf.resize(index as usize * LFN_PART_LEN, 0);
        } else if self.index == 0 || index != self.index - 1 || data.checksum != self.chksum {
            // Corrupted entry
            warn!("currupted lfn entry! {:x} {:x} {:x} {:x}", data.order, self.index, data.checksum, self.chksum);
            self.clear();
            return;
        } else {
            // Decrement LFN index only for non-last entries
            self.index -= 1;
        }
        let pos = LFN_PART_LEN * (index - 1) as usize;
        // copy name parts into LFN buffer
        self.buf[pos+0..pos+5].copy_from_slice(&data.name_0);
        self.buf[pos+5..pos+11].copy_from_slice(&data.name_1);
        self.buf[pos+11..pos+13].copy_from_slice(&data.name_2);
    }
    
    fn validate_chksum(&mut self, short_name: &[u8]) {
        let chksum = lfn_checksum(short_name);
        if chksum != self.chksum {
            warn!("checksum mismatch {:x} {:x} {:?}", chksum, self.chksum, short_name);
            self.clear();
        }
    }
}
