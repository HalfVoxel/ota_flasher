use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use crate::OtaUpdateFromUrl;
use brevduva::{
    channel::{ChannelMessage, SerializationFormat},
    SyncStorage,
};
use esp_idf_svc::{
    http::{client::EspHttpConnection, Method},
    ota::EspOta,
    sys::EspError,
};
use log::{error, info};

pub async fn initialize_ota(storage: &SyncStorage, device_id: &str, firmware_version: &str) {
    let (ota_progress, _) = storage
        .add_channel(
            &format!("devices/{device_id}/ota/progress"),
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let (_, mut firware_channel) = storage
        .add_channel::<OtaUpdateFromUrl>(
            &format!("devices/{device_id}/ota/firmware"),
            SerializationFormat::Json,
        )
        .await
        .unwrap();

    let _firware_version_container = storage
        .add_container_with_mode::<String>(
            &format!("devices/{device_id}/version"),
            firmware_version.to_owned(),
            SerializationFormat::Json,
            brevduva::ReadWriteMode::Driven,
        )
        .await
        .unwrap();

    tokio::spawn(async move {
        loop {
            if let Err(e) = try_receive_firmware(&mut firware_channel, &ota_progress).await {
                error!("Error while receiving firmware: {:?}", e);
            }
        }
    });
}

#[derive(thiserror::Error, Debug)]
enum FirmwareError {
    #[error("Failed to download firmware: {0}")]
    Custom(&'static str),

    #[error("Received {received_bytes} bytes of firmware data, but expected {expected_bytes}")]
    WrongLength {
        expected_bytes: u32,
        received_bytes: u32,
    },
    #[error(transparent)]
    EspError(#[from] EspError),
}

async fn try_receive_firmware(
    firware_channel: &mut tokio::sync::mpsc::Receiver<ChannelMessage<OtaUpdateFromUrl>>,
    ota_progress: &Arc<brevduva::channel::Channel<u32>>,
) -> Result<(), FirmwareError> {
    info!("Monitoring OTA...");
    if let Some(ChannelMessage {
        message: OtaUpdateFromUrl {
            url,
            size: expected_bytes,
        },
        ..
    }) = firware_channel.recv().await
    {
        info!("Begin OTA update");
        ota_progress.send(0).await;

        let ota_progress = (*ota_progress).clone();
        let update_task = thread::Builder::new().stack_size(4096).spawn(move || {
            // let update_task = tokio::task::spawn_blocking(move || {
            info!("Starting OTA update from {url}");
            let mut ota = EspOta::new().expect("obtain OTA instance");
            let mut update = ota.initiate_update().expect("initiate OTA");

            let mut client =
                EspHttpConnection::new(&esp_idf_svc::http::client::Configuration {
                    crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
                    buffer_size: Some(32*1024),
                    ..Default::default()
                })?;

            client.initiate_request(Method::Get, &url, &[])?;

            client.initiate_response()?;

            if client.status() != 200 {
                return Err(FirmwareError::Custom(
                    "Failed to download firmware. Non-200 status",
                ));
            }

            let mut received_bytes = 0u32;

            let mut buffer = vec![0u8; 16*1024];
            let mut last_update = Instant::now();
            loop {
                let n_bytes_read = client.read(&mut buffer)?;

                if n_bytes_read == 0 {
                    break;
                }

                received_bytes += n_bytes_read as u32;
                update.write(&buffer[..n_bytes_read])?;

                if last_update.elapsed() > Duration::from_millis(200) {
                    let progress_percent = (received_bytes * 100) / expected_bytes;
                    ota_progress.blocking_send(progress_percent);
                    last_update = Instant::now();
                    info!("Flashing progress: {progress_percent}%: {received_bytes}/{expected_bytes} bytes");

                    // Allow other tasks to run
                    thread::yield_now();
                }
            }

            if received_bytes == expected_bytes {
                update.complete()?;

                ota_progress.blocking_send(100);
                info!("Rebooting to apply update");
                // Sleep for a bit to allow the progress to be sent
                // TODO: Monitor ACKs from server?
                thread::sleep(Duration::from_millis(100));

                esp_idf_svc::hal::reset::restart();
            } else {
                update.abort()?;
                Err::<(), FirmwareError>(FirmwareError::WrongLength {
                    expected_bytes,
                    received_bytes,
                })
            }
        });

        let update_task = update_task.unwrap();
        loop {
            if update_task.is_finished() {
                update_task
                    .join()
                    .map_err(|_e| FirmwareError::Custom("Join error"))??;
                break;
            } else {
                // tokio sleep
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    Ok(())
}
