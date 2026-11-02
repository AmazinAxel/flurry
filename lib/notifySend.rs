// flurry's own notifications go straight to the in-process daemon; action argvs run on invoke.

use crate::lib::notifd;

pub fn notify_send(app: &str, title: &str, body: &str, actions: Vec<(u32, String, Vec<String>)>) -> u32 {
    let list: Vec<(String, String)> = actions.iter().map(|(id, label, _)| (id.to_string(), label.clone())).collect();
    notifd::send_local(app, title, body, &list, move |action| {
        if let Some((_, _, argv)) = actions.iter().find(|(id, _, _)| id.to_string() == action)
            && !argv.is_empty()
        {
            crate::lib::apps::launch(argv);
        }
    })
}
