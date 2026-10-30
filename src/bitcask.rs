use fs4::FileExt;
use std::{
    collections::btree_map,
    io::{BufReader, BufWriter, Read, Write, SeekFrom, Seek},
    ops::Bound,
    path::PathBuf,
};

const KEY_VAL_HEADER_LEN: u32 = 4;
const MERGE_FILE_EXT: &str = "merg";

type KeyDir = std::collections::BTreeMap<Vec<u8>, (u64, u32)>;

pub type Result<T> = std::result::Result<T, std::io::Error>;

pub struct Bitcask {
    log: Log,
    keydir: KeyDir,
}

impl Drop for Bitcask {
    fn drop(&mut self) {
        match self.log.file.flush() {
            Ok(()) => {}
            Err(error) => {
                log::error!("failed to flush file: {:?}", error);
            }
        }
    }
}

impl Bitcask {
    pub fn new(path: PathBuf) -> Result<Self> {
        let mut log = Log::new(path)?;
        let keydir = log.load_index()?;
        Ok(Self {log, keydir})
    }

    pub fn merge(&mut self) -> Result<()> {
        let mut merge_path = self.log.path.clone();
        merge_path.set_extension(MERGE_FILE_EXT);

        let mut new_log = Log::new(merge_path)?;
        let mut new_keydir = KeyDir::new();

        for (key, (value_pos, value_len)) in self.keydir.iter() {
            let value = self.log.read_value(*value_pos, *value_len)?;
            let (offset, len) = new_log.write_entry(key, Some(value.as_slice()))?;
            //计算在新文件中,value的位置，插入新索引
            new_keydir.insert(
                key.clone(),
                (offset + len as u64 - *value_len as u64, *value_len),
            );
        }

        std::fs::rename(&new_log.path, &self.log.path)?;
        new_log.path = self.log.path.clone();

        self.log = new_log;
        self.keydir = new_keydir;

        Ok(())
    }

    pub fn set(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        //1、把记录追加到日志文件末尾，得到（起始偏移，记录总长）
        let (offset, len) = self.log.write_entry(key, Some(&value))?;
        let value_len = value.len() as u32;
        //2、更新内存索引
        self.keydir.insert(
            key.to_vec(),
            (offset + len as u64 - value_len as u64, value_len),
        );
        Ok(())
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some((value_pos, value_len)) = self.keydir.get(key) {
            let val = self.log.read_value(*value_pos, *value_len)?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.log.write_entry(key, None)?;
        self.keydir.remove(key);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(self.log.file.sync_all()?)
    }

    pub fn scan(&mut self, range: impl std::ops::RangeBounds<Vec<u8>>) -> ScanIterator<'_> {
        ScanIterator {
            inner: self.keydir.range(range),
            log: &mut self.log,
        }
    }

    pub fn scan_prefix(&mut self, prefix: &[u8]) -> ScanIterator<'_> {
        let start = Bound::Included(prefix.to_vec()); // 左边界：包含 prefix 本身

        let mut bound_prefix = prefix.to_vec().clone();
        if let Some(last) = bound_prefix.iter_mut().last() {
            *last += 1;
        };
        let end = Bound::Excluded(bound_prefix.to_vec()); // 右边界：不包含 prefix+1

        // 复用范围扫描
        self.scan((start, end))
    }

}

pub struct ScanIterator<'a> {
    inner: btree_map::Range<'a, Vec<u8>, (u64, u32)>,
    log: &'a mut Log,
}

impl <'a> ScanIterator<'a> {
    fn map(&mut self, item: (&Vec<u8>, &(u64, u32))) -> <Self as Iterator>::Item {
        let (key, (value_pos, value_len)) = item;
        let value = self.log.read_value(*value_pos,*value_len)?;
        Ok((key.clone(), value))
    }
}

impl <'a> Iterator for ScanIterator<'a> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|item|self.map(item))
    }
}

impl <'a> DoubleEndedIterator for ScanIterator<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|item|self.map(item))
    }
}

struct Log {
    path: PathBuf,
    file: std::fs::File,
}

impl Log {
    fn new(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        file.try_lock_exclusive()?;

        Ok(Self { path, file })
    }

    fn load_index(&mut self) -> Result<KeyDir> {
        let mut len_buf = [0; KEY_VAL_HEADER_LEN as usize];
        let mut keydir = KeyDir::new();
        let file_len = self.file.metadata()?.len();
        let mut r = BufReader::new(&mut self.file);
        let mut pos: u64 = r.seek(SeekFrom::Start(0))?;

        while pos < file_len {
            let read_one = || -> Result<(Vec<u8>, u64, Option<u32>)> {
                //读4字节的key长度
                r.read_exact(&mut len_buf)?;
                let key_len = u32::from_be_bytes(len_buf);
                //读4字节的value长度
                r.read_exact(&mut len_buf)?;
                let value_lent_or_tombstone = match i32::from_be_bytes(len_buf) {
                    l if l >= 0 => Some(l as u32),
                    _ => None,
                };

                //推到value的位置
                let value_pos = pos + KEY_VAL_HEADER_LEN as u64 * 2 + key_len as u64;

                //读取key的内容
                let mut key = vec![0; key_len as usize];
                r.read_exact(&mut key)?;

                if let Some(value_len) = value_lent_or_tombstone {
                    r.seek_relative(value_len as i64)?;
                }

                Ok((key, value_pos, value_lent_or_tombstone))
            }();

            match read_one {
                Ok((key, value_pos, Some(value_len))) => {
                    keydir.insert(key, (value_pos, value_len));
                    pos = value_pos + value_len as u64;
                }
                Ok((key, value_pos, None)) => {
                    keydir.remove(&key);
                    pos = value_pos;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(keydir)
    }

    fn read_value(&mut self, value_pos: u64, value_len: u32) -> Result<Vec<u8>> {
        let mut value = vec![0; value_len as usize];
        self.file.seek(SeekFrom::Start(value_pos))?;
        self.file.read_exact(&mut value)?;
        Ok(value)
    }

    fn write_entry(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<(u64, u32)> {
        let key_len = key.len() as u32;
        let value_len = value.map_or(0, |v| v.len() as u32);
        let value_len_or_tombstone = value.map_or(-1, |v| v.len() as i32);

        let len = KEY_VAL_HEADER_LEN * 2 + key_len + value_len;

        let offset = (self.file).seek(SeekFrom::End(0))?;
        let mut w = BufWriter::with_capacity(len as usize, &mut self.file);

        w.write_all(&key_len.to_be_bytes())?;
        w.write_all(&value_len_or_tombstone.to_be_bytes())?;
        w.write_all(key)?;
        if let Some(v) = value {
            w.write_all(v)?;
        }
        w.flush()?;

        Ok((offset, len))
    }
}