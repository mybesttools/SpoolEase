use core::{cell::RefCell, num::ParseIntError};

use framework::prelude::{SDCardStore, SDCardStoreErrorSource};
use log::info;

use alloc::{format, rc::Rc, string::String, vec::Vec};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embedded_hal_async::spi::SpiDevice;
use hashbrown::HashMap;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

// use crate::sdcard_store::{SDCardStore, SDCardStoreErrorType};

use snafu::prelude::*;

#[derive(Snafu, Debug)]
pub enum CsvDbError {
    #[snafu(display("Failed to open volume"))]
    Store {
        source: SDCardStoreErrorSource,
    },

    Metadata {
        source: ParseIntError,
    },

    Deserialize {
        source: serde_csv_core::de::Error,
    },

    Serialize {
        source: serde_csv_core::ser::Error
    },
}

pub trait CsvDbId {
    fn id(&self) -> &String;
}

#[derive(Debug)]
pub struct CsvRecordInfo<T>
where
    T: PartialEq + core::fmt::Debug,
{
    pub data: T,
    pub length: usize, // including EOL (\n at this time, so +1)
    offset: u32,
}

#[derive(Serialize, Deserialize)]
struct DbMeta {
    version: usize,
    record_width: usize,
}

struct CsvDbInner
{
    db_file_name: String,
    _dbm_file_name: String,
    record_width: usize,
}

pub struct CsvDb<T, SPI: SpiDevice, const MAX_DIRS: usize, const MAX_FILES: usize>
where
    T: CsvDbId + Serialize + DeserializeOwned + PartialEq + core::fmt::Debug,
{
    sdcard: Rc<Mutex<CriticalSectionRawMutex, SDCardStore<SPI, MAX_DIRS, MAX_FILES>>>,
    inner: RefCell<CsvDbInner>,
    pub records: Rc<RefCell<HashMap<String, CsvRecordInfo<T>>>>,
}

impl<T, SPI: SpiDevice, const MAX_DIRS: usize, const MAX_FILES: usize> CsvDb<T, SPI, MAX_DIRS, MAX_FILES>
where
    T: CsvDbId + Serialize + DeserializeOwned + PartialEq + core::fmt::Debug,
{
    pub async fn new(
        sdcard: Rc<Mutex<CriticalSectionRawMutex, SDCardStore<SPI, MAX_DIRS, MAX_FILES>>>,
        db_name: &str,
        min_record_width: usize,
        min_capacity: usize,
    ) -> Result<Self, CsvDbError> {
        let dbm_file_name = format!("{db_name}.dbm");
        let db_file_name = format!("{db_name}.db");
        let sdcard_input = sdcard.clone();
        let mut record_width = min_record_width;
        let mut records = HashMap::<String, CsvRecordInfo<T>>::with_capacity(min_capacity);

        let mut sdcard = sdcard.lock().await;
        let dbm_str = sdcard.read_create_str(&dbm_file_name).await.context(StoreSnafu)?;
        if dbm_str.is_empty() {
            let dbm_str = format!("version: 1\nrecord_width:{min_record_width}");
            sdcard.append_text(&dbm_file_name, &dbm_str).await.context(StoreSnafu)?;
            sdcard.create_file(&db_file_name).await.context(StoreSnafu)?;
        } else {
            // Get relevant info from dbm file
            let mut lines = dbm_str.lines();
            let _line1 = lines.next();
            let line2 = lines.next();
            if let Some(line2) = line2 {
                if let Some((_left, right)) = line2.split_once(':') {
                    record_width = right.trim().parse().context(MetadataSnafu)?;
                }
            }
            // Now read db file
            let db_bytes = sdcard.read_create_bytes(&db_file_name).await.context(StoreSnafu)?;
            let mut reader = serde_csv_core::Reader::<100>::new(); // 100 is max field size
            let mut nread = 0;
            while nread < db_bytes.len() {
                let db_record = &db_bytes[nread..nread + record_width];
                if !Self::is_empty_record(db_record) {
                    let (record, record_length) = reader.deserialize::<T>(db_record).context(DeserializeSnafu)?;
                    let record_info = CsvRecordInfo {
                        data: record,
                        offset: nread as u32,
                        length: record_length
                    };
                    records.insert(record_info.data.id().clone(), record_info);
                }
                nread += record_width;
            }
            info!("Done reading: {records:?}");
        }

        Ok(Self {
            inner: RefCell::new(CsvDbInner {
                db_file_name,
                _dbm_file_name: dbm_file_name,
                record_width,
            }),
            sdcard: sdcard_input.clone(),
            records: Rc::new(RefCell::new(records)),
        })
    }

    pub async fn insert(&self, record: T) -> Result<(), CsvDbError> {
        let (already_exist, offset) = if let Some(v) = self.records.borrow().get(record.id()) {
            if v.data == record {
                return Ok(());
            }
            (true, v.offset)
        } else {
            (false, 0)
        };
        let mut buffer = Vec::<u8>::with_capacity(self.inner.borrow().record_width);
        let serialized_len = self.calc_csv_row(&record, &mut buffer)?;
        let db_file_name = self.inner.borrow().db_file_name.clone();
        if already_exist {
            let mut sdcard = self.sdcard.lock().await;
            sdcard
                .write_file_bytes(&db_file_name, offset, buffer.as_slice())
                .await
                .context(StoreSnafu)?;
            if let Some(v) = self.records.borrow_mut().get_mut(record.id()) {
                v.data = record;
                v.length = serialized_len;
            }
        } else {
            let mut sdcard = self.sdcard.lock().await;
            let offset = sdcard
                .append_bytes(&db_file_name, buffer.as_slice())
                .await
                .context(StoreSnafu)?;
            let csv_record_info = CsvRecordInfo { data: record, offset, length: serialized_len };
            self.records.borrow_mut().insert(csv_record_info.data.id().clone(), csv_record_info);
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn delete(self, id: &str) -> Result<Option<T>, CsvDbError> {
        let offset = if let Some(v) = self.records.borrow().get(id) {
            v.offset
        } else {
            return Ok(None);
        };

        let mut buffer = Vec::<u8>::with_capacity(self.inner.borrow().record_width);
        self.calc_empty_record(&mut buffer);
        let mut sdcard = self.sdcard.lock().await;
        let db_file_name = self.inner.borrow().db_file_name.clone();
        sdcard
            .write_file_bytes(&db_file_name, offset, buffer.as_slice())
            .await
            .context(StoreSnafu)?;

        if let Some(record) = self.records.borrow_mut().remove(id) {
            return Ok(Some(record.data));
        } 
        Ok(None)
    }

    fn calc_csv_row(&self, record: &T, buffer: &mut Vec<u8>) -> Result<usize, CsvDbError> {
        let mut writer = serde_csv_core::Writer::new();
        self.calc_empty_record(buffer);
        let length_written = writer.serialize(record, buffer.as_mut_slice()).context(SerializeSnafu)?;
        Ok(length_written)
    }

    fn calc_empty_record(&self, buffer: &mut Vec<u8>) {
        let record_width = self.inner.borrow().record_width;
        buffer.resize(record_width, b'-');
        buffer[record_width - 1] = b'\n';
    }

    fn is_empty_record(s: &[u8]) -> bool {
        s.len() > 1 && s[s.len() - 1] == b'\n' && s[..s.len() - 1].iter().all(|&c| c == b'-')
    }
}
