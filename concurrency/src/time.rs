use std::time::Duration;

use spawned_rt::{self as rt, JoinHandle, mpsc::Sender};

use crate::{GenServer, GenServerInMsg};

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
    rt::spawn(async move {
        rt::sleep(period).await;
        let _ = tx.send(GenServerInMsg::Cast { message });
    })
}
