use std::time::Duration;

use spawned_rt::sync::{self as rt, JoinHandle, mpsc::Sender};

use super::gen_server::{GenServer, GenServerInMsg};

// Sends a message after a given period to the specified GenServer. The task terminates
// once the send has completed
pub fn send_after<T>(
    period: Duration,
    tx: Sender<GenServerInMsg<T>>,
    message: T::InMsg,
) -> JoinHandle<()>
where
    T: GenServer + 'static,
{
    rt::spawn(move || {
        rt::sleep(period);
        let _ = tx.send(GenServerInMsg::Cast { message });
    })
}
