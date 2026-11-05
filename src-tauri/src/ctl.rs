#[cfg(target_os = "linux")]
fn main() {
    use voice_platform::ipc::{ControlAction, ControlRequest, ReplyCode, send_request};
    let action = match std::env::args().nth(1).as_deref() {
        Some("start") => ControlAction::Start,
        Some("stop") => ControlAction::Stop,
        Some("toggle") => ControlAction::Toggle,
        Some("cancel") => ControlAction::Cancel,
        Some("status") => ControlAction::Status,
        _ => {
            eprintln!("用法：voice-inputctl start|stop|toggle|cancel|status");
            std::process::exit(2);
        }
    };
    match send_request(ControlRequest { action }) {
        Ok(reply) => {
            println!("{}", serde_json::to_string(&reply).expect("control reply"));
            if !matches!(reply.code, ReplyCode::Accepted) {
                std::process::exit(1);
            }
        }
        Err(_) => {
            eprintln!("声笺未运行，或当前登录会话的控制接口不可用。");
            std::process::exit(1);
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("当前控制客户端仅适用于 Linux 登录会话。");
    std::process::exit(1);
}
