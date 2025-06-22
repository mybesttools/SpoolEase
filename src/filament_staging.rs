use crate::bambu::TagInformation;

pub struct FilamentStaging {
    tag_info: Option<TagInformation>,
    origin: StagingOrigin,
}

#[derive(PartialEq)]
pub enum StagingOrigin {
    Empty,
    Scanned,
    Encoded,
    Unloaded,
}

impl FilamentStaging {
    pub fn new() -> Self {
        Self { tag_info: None, origin: StagingOrigin::Empty }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tag_info.is_none()
    }

    pub fn clear(&mut self) {
        self.tag_info = None;
        self.origin = StagingOrigin::Empty;
    }
    pub fn tag_info(&self) -> &Option<TagInformation> {
        &self.tag_info
    }
    pub fn set_tag_info(&mut self, tag_info: TagInformation, origin: StagingOrigin) {
        self.tag_info = Some(tag_info);
        self.origin = origin;
    }
    pub fn origin(&self) -> &StagingOrigin {
        &self.origin
    }
    
}
