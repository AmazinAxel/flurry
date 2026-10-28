// flurry's own notifications go straight to the in-process daemon; action commands run on invoke.

use crate::lib::notifd;
use crate::lib::process::exec_str;

pub fn notify_send(app_name: &str, title: &str, body: &str, actions: Vec<(u32, String, String)>) -> u32 {
    let list: Vec<(String, String)> = actions.iter().map(|(id, label, _)| (id.to_string(), label.clone())).collect();
    notifd::send_local(app_name, title, body, &list, move |action| {
        if let Some((_, _, cmd)) = actions.iter().find(|(id, _, _)| id.to_string() == action) {
            let cmd = cmd.clone();
            crate::ui::runtime::spawn(async move {
                let _ = exec_str(&cmd).await;
            });
        }
    })
}
