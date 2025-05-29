use std::time::Duration;

use spawned_rt::threads::{self as rt, mpsc::Sender, JoinHandle};

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
