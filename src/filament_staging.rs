use alloc::rc::Rc;

use crate::{bambu::TagInformation, store::{SpoolRecord, Store}};

pub struct FilamentStaging {
    tag_info: Option<TagInformation>,
    spool_record: Option<SpoolRecord>,
    origin: StagingOrigin,
    store: Rc<Store>
}

#[derive(PartialEq)]
pub enum StagingOrigin {
    Empty,
    Scanned,
    Encoded,
    Unloaded,
}

impl FilamentStaging {
    pub fn new(store: Rc<Store>) -> Self {
        Self { tag_info: None, spool_record: None, origin: StagingOrigin::Empty, store: store.clone() }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tag_info.is_none()
    }

    pub fn clear(&mut self) {
        self.tag_info = None;
        self.spool_record = None;
        self.origin = StagingOrigin::Empty;
    }
    pub fn tag_info(&self) -> &Option<TagInformation> {
        &self.tag_info
    }
    pub fn spool_record(&self) -> &Option<SpoolRecord> {
        &self.spool_record
    }
    pub fn tag_info_mut(&mut self) -> &mut Option<TagInformation> {
        &mut self.tag_info
    }
    pub fn set_tag_info(&mut self, mut tag_info: TagInformation, origin: StagingOrigin) {
        // if loaded in scanning scenario or unloading scenario, the store should reflect some of the fields
        // also store_record is automatically fetched if available
        if [StagingOrigin::Scanned, StagingOrigin::Unloaded].contains(&origin) {
            if let Some(tag_id) = &tag_info.tag_id {
                if let Some(spool_in_store) = self.store.get_spool_by_tag_id(tag_id) {
                    tag_info.note = Some(spool_in_store.note.clone());
                    self.spool_record = Some(spool_in_store);
                }
            }
        }
        self.tag_info = Some(tag_info);
        self.origin = origin;
    }
    pub fn origin(&self) -> &StagingOrigin {
        &self.origin
    }
    
}
