use std::io::{self, Write};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, anyhow};
use prost::Message;
use serde_json::json;
use tokio::time::Instant;
use tracing::info;
use zenoh::bytes::ZBytes;
use zenoh::config::ZenohId;

use crate::proto::v1::*;
static ZENOH_SESSION_USB: once_cell::sync::OnceCell<zenoh::Session> =
    once_cell::sync::OnceCell::new();

pub const KEY_APPLY: &str = "arkkvm/usb_devices/query/apply_switches";
pub const KEY_GET: &str = "arkkvm/usb_devices/query/get_switches";

//  dd if=/dev/zero of="/root/ft_disk.img" bs=1M count=0 seek=50 
//  mkfs.vfat -F 32 "/root/ft_disk.img"

pub const VM_ISO_PATH:&str = "/root/steam.iso";
pub const FT_IMG_PATH:&str = "/root/ft_disk.img";

pub async fn test_task() {
    // Test code here
    println!("Running USB device tests");

    zenoh_init().await.expect("Failed to initialize zenoh for testing");
    // send_with_reply(ApplySwitchesRequest {
    //     usb_info: Some(UsbDeviceInfo { ..Default::default() }),
    // })
    // .await
    // .expect("Failed to send apply switches request");

 

    loop {
        sleep(Duration::from_secs(2));

        print_param_hint();
        print!("Enter parameters (Enter to run, Ctrl+C to exit): ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        let new_args: Vec<String> = input.split_whitespace().map(|s| s.to_string()).collect();

        let usb_info = build_usb_info_from_args(&new_args);

        println!("Applying USB switch config: {:?}", usb_info);

        send_with_reply(ApplySwitchesRequest { usb_info: Some(usb_info) })
            .await
            .expect("Failed to send apply switches request");
    }
}

pub async fn send_with_reply(req: ApplySwitchesRequest) -> Result<ApplySwitchesResponse> {
    let mut buf = Vec::new();
    req.encode(&mut buf)?;

    // Zenoh GET (not PUT)
    let replies = get_usb_session()
        .get(KEY_APPLY)
        .payload(ZBytes::from(buf))
        .await
        .map_err(|e| anyhow::anyhow!("zenoh get failed: {}", e))?;

    // wait for the first reply
    if let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            let bytes = sample.payload().to_bytes();
            let resp = ApplySwitchesResponse::decode(&bytes[..])?;
            println!("Config applied: {:?}", resp);
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("config apply failed"))
        }
    } else {
        Err(anyhow::anyhow!("no reply received"))
    }
}

pub async fn zenoh_init() -> Result<()> {
    let session_usb =
        create("/tmp/zenoh_usb_devices.sock").await.expect("Failed to create usb zenoh session");

    if let Err(e) = ZENOH_SESSION_USB.set(session_usb) {
        return Err(anyhow!("Failed to set zenoh session: {e:?}"));
    }

    Ok(())
}

pub fn get_usb_session() -> zenoh::Session {
    ZENOH_SESSION_USB.get().expect("Zenoh session not initialized").clone()
}

async fn create(sock_path: &str) -> Result<zenoh::Session> {
    zenoh::init_log_from_env_or("debug");

    info!("Opening session...");
    let mut config = zenoh::Config::default();
    let endpoint = format!("unixsock-stream/{}", sock_path);
    if let Err(e) = config.insert_json5("listen/endpoints", &format!(r#"["{}"]"#, endpoint)) {
        return Err(anyhow!("Failed to insert listen/endpoints: {e:?}"));
    }

    if let Err(e) = config.insert_json5("scouting/multicast/enabled", &json!(false).to_string()) {
        return Err(anyhow!("Failed to disable multicast scouting: {e:?}"));
    }

    if let Err(e) = config.insert_json5("connect/endpoints", r#"[]"#) {
        return Err(anyhow!("Failed to set connect/endpoints: {e:?}"));
    }

    if let Err(e) = config.insert_json5("transport/shared_memory/enabled", &json!(true).to_string())
    {
        return Err(anyhow!("Failed to insert shared memory config: {e:?}"));
    }

    let session = zenoh::open(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open zenoh session: {}", e))?;

    let info = session.info();
    info!("zid: {}", info.zid().await);
    info!("routers zid: {:?}", info.routers_zid().await.collect::<Vec<ZenohId>>());
    info!("peers zid: {:?}", info.peers_zid().await.collect::<Vec<ZenohId>>());

    return Ok(session);
}

fn build_usb_info_from_args(args: &[String]) -> UsbDeviceInfo {
    let mut info = UsbDeviceInfo::default();
    let mut switchs = info.switches.unwrap_or_default();
    // take at most the first 5 arguments
    for (i, v) in args.iter().enumerate().take(5) {
        let enable = v == "1";
        match i {
            0 => switchs.hid_kb_rel_enabled = enable,
            1 => switchs.hid_abs_enabled = enable,
            2 => switchs.ums_vm_enabled = enable,
            3 => switchs.ums_ft_enabled = enable,
            4 => switchs.uac1_enabled = enable,
            _ => {}
        }
    }

    info.switches = Some(switchs);
    info.ums_vm_path = VM_ISO_PATH.to_string();
    info.ums_ft_path = FT_IMG_PATH.to_string();
    info.ums_vm_type = UmsVmType::VmCdRom as i32;
    info
}

// print current parameter help
fn print_param_hint() {
    println!("Parameters (0=off, 1=on):");
    println!("  [0] hid_kb_rel_enabled : keyboard + relative mouse");
    println!("  [1] hid_abs_enabled    : absolute mouse");
    println!("  [2] ums_vm_enabled     : virtual media");
    println!("  [3] ums_ft_enabled     : file transfer");
    println!("  [4] uac1_enabled       : virtual microphone");
    println!("Example: 1 0 1 0 1");
}
