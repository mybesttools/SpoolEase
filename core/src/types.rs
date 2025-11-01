use alloc::string::String;

#[allow(dead_code)]
pub struct FilamentSupInfo {
    pub origin_is_material: bool,
    pub base_filament: bool,
    pub slicer_name: String,
    pub slicer_code: String,
    pub nozzle_temp_low: i32,
    pub nozzle_temp_high: i32,
}
