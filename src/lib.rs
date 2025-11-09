#[cfg(feature = "embedded")]
pub mod downloader;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct OtaUpdateFromUrl {
    pub url: String,
    pub size: u32,
}
