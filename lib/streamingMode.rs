use crate::lib::state::State;

thread_local! {
    static STREAMING: State<bool> = State::new(false);
}

pub fn streaming_mode() -> State<bool> {
    STREAMING.with(Clone::clone)
}
