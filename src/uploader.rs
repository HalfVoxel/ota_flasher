use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use brevduva::{
    channel::{ChannelMessage, SerializationFormat},
    SyncStorage,
};
use clap::{error, Parser};
use headway::ProgressBar;
use local_ip_address::local_ip;
use ota_flasher::OtaUpdateFromUrl;

const MQTT_HOST: &str = "mqtt://arongranberg.com:1883";
const MQTT_CLIENT_ID: &str = "ota_flasher";
const MQTT_USERNAME: &str = "wakeup_alarm";
const MQTT_PASSWORD: &str = "xafzz25nomehasff";

#[tokio::main]
pub async fn main() {
    let args = Args::parse();
    upload(args.device, &args.image, &args.version_file).await;
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Name of the device to upload to
    #[arg(long)]
    device: String,
    #[arg(long)]
    image: PathBuf,
    #[arg(long)]
    version_file: PathBuf,
}

pub async fn upload(device: String, image: &Path, version_file: &Path) {
    env_logger::init();
    // esp_idf_svc::log::set_target_level("", log::LevelFilter::Debug).unwrap();

    let storage = SyncStorage::new(
        MQTT_CLIENT_ID,
        MQTT_HOST,
        MQTT_USERNAME,
        MQTT_PASSWORD,
        brevduva::SessionPersistance::Transient,
    )
    .await;

    let devices = available_devices(&storage).await;
    if !devices.contains(&device) {
        println!("Device not online");
        println!("Available devices:");
        for device in devices {
            println!("\t{device}");
        }
        return;
    }

    println!("Found device");

    let (firmware_channel, _) = storage
        .add_channel::<OtaUpdateFromUrl>(
            &format!("devices/{device}/ota/firmware"),
            SerializationFormat::Json,
        )
        .await
        .unwrap();

    let (_, progress_channel) = storage
        .add_channel::<u32>(
            &format!("devices/{device}/ota/progress"),
            SerializationFormat::Auto,
        )
        .await
        .unwrap();

    let new_firmware_version =
        std::fs::read_to_string(version_file).expect("Could not read version file");

    let (_, mut firware_version_container) = storage
        .add_channel::<String>(
            &format!("devices/{device}/version"),
            SerializationFormat::Json,
        )
        .await
        .unwrap();
    let previous_firmware = firware_version_container.recv().await.unwrap().message;

    if new_firmware_version.trim() == previous_firmware.trim() {
        println!("Firmware is already up to date");
        return;
    }

    println!("Updating firmware\n\tfrom: {previous_firmware}\n\tto:   {new_firmware_version}");

    let firmware = std::fs::read(image).expect("Could not read firmware file");
    let size = firmware.len() as u32;
    let (url, stop_server) = start_file_server(firmware, 8123);

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        firmware_channel
            .send(OtaUpdateFromUrl {
                url: url.clone(),
                size,
            })
            .await;
    });

    if let Err(e) = monitor_progress(progress_channel).await {
        println!("Failed to upload firmware: {:?}", e);
    }
    stop_server.send(()).unwrap();

    println!("Waiting for device to reboot");

    tokio::select! {
        r = wait_for_new_version(&mut firware_version_container, &new_firmware_version) => {
            match r {
                Ok(()) => {
                    println!("Firmware updated");
                }
                Err(version) => {
                    println!("Firmware did not update. Device rebooted with incorrect version.");
                    println!("\tFound on device:  {version}");
                    println!("\tExpected version: {new_firmware_version}");
                }
            }
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
            println!("Timeout waiting for device to reboot");
        }
    }
}

async fn wait_for_new_version(
    firware_version_container: &mut tokio::sync::mpsc::Receiver<ChannelMessage<String>>,
    new_firmware_version: &str,
) -> Result<(), String> {
    if let Some(ChannelMessage { message, .. }) = firware_version_container.recv().await {
        if message == new_firmware_version {
            return Ok(());
        } else {
            return Err(message);
        }
    }
    panic!("Channel closed");
}

async fn monitor_progress(
    mut progress_channel: tokio::sync::mpsc::Receiver<ChannelMessage<u32>>,
) -> Result<(), ()> {
    let start_time = std::time::Instant::now();
    let mut bar = ProgressBar::new()
        .with_length(100)
        .with_message("Uploading firmware");
    loop {
        tokio::select! {
            message = progress_channel.recv() => {
                if let Some(ChannelMessage {
                    message: progress, ..
                }) = message {
                    bar.set_position(progress as usize);
                    if progress == 100 {
                        bar.finish_with_message(format!("Uploaded firmware in {:.1?}", start_time.elapsed()));
                        return Ok(());
                    }
                } else {
                    return Err(());
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                return Err(());
            }
        }
    }
}

async fn available_devices(storage: &SyncStorage) -> Vec<String> {
    let (data, mut channel) = storage
        .add_channel::<String>("devices/+/status", SerializationFormat::Auto)
        .await
        .unwrap();

    let mut v = vec![];
    loop {
        tokio::select! {
            item = channel.recv() => {
                if let Some(item) = item {
                    v.push(item);
                } else {
                    break;
                }
            }
            _ = data.wait_for_sync() => {
                break;
            }
        }
    }

    while let Ok(item) = channel.try_recv() {
        v.push(item);
    }

    let devices = v
        .into_iter()
        .filter(|x| x.message == "online")
        .map(|x| x.topic)
        .map(|x| x.split('/').nth(2).unwrap().to_owned())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|x| x != MQTT_CLIENT_ID)
        .collect::<Vec<_>>();

    // Drain the channel forever. To avoid it blocking other parts of the program
    tokio::spawn(async move { while let Some(_item) = channel.recv().await {} });

    devices
}

fn start_file_server(firmware: Vec<u8>, port: u32) -> (String, std::sync::mpsc::Sender<()>) {
    let ip = local_ip().unwrap();
    let url = format!("http://{ip}:{port}/firmware.bin");
    // println!("Starting server at {url}");

    let (_, stop_server) = rouille::Server::new(format!("0.0.0.0:{port}"), move |request| {
        if request.url() != "/firmware.bin" {
            return rouille::Response::empty_404();
        }

        rouille::Response::from_data("application/octet-stream", firmware.clone())
    })
    .unwrap()
    .stoppable();

    (url, stop_server)
}
// fn do_stuff() {
//     let sender = tokio::spawn(async move {
//         firmware_channel
//             .send(OtaFirmwarePart::Begin {
//                 size: firmware.len() as u32,
//             })
//             .await;

//         // for chunk in firmware.chunks(512) {
//         //     println!("Sending chunk: {}", chunk.len());
//         //     firmware_channel
//         //         .send(OtaFirmwarePart::Data(chunk.to_vec()))
//         //         .await;
//         // }

//         // firmware_channel
//         //     .send(OtaFirmwarePart::End {
//         //         hash: 0xdeadbeefdeadbeef,
//         //     })
//         //     .await;
//     });

//     async fn monitor_progress(
//         mut progress_channel: tokio::sync::mpsc::Receiver<ChannelMessage<u32>>,
//     ) -> Result<(), ()> {
//         loop {
//             tokio::select! {
//                 message = progress_channel.recv() => {
//                     if let Some(ChannelMessage {
//                         message: progress, ..
//                     }) = message {
//                         println!("Progress: {}", progress);
//                         if progress == 100 {
//                             return Ok(());
//                         }
//                     } else {
//                         return Err(());
//                     }
//                 }
//                 _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
//                     return Err(());
//                 }
//             }
//         }
//     }

//     let (r, r2) = tokio::join!(sender, monitor_progress(progress_channel));
//     r.unwrap();
//     r2.unwrap();
// }
