use std::time::Duration;

use spawned_rt::threads::{self as rt, JoinHandle};

use super::{GenServer, GenServerHandle};

// Sends a message after a given period to the specified GenServer. The task terminates
// once the send has completed
pub fn send_after<T>(
    period: Duration,
    mut handle: GenServerHandle<T>,
    message: T::CastMsg,
) -> JoinHandle<()>
where
    T: GenServer + 'static,
{
    rt::spawn(move || {
        rt::sleep(period);
        let _ = handle.cast(message);
    })
}
