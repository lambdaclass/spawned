//! Simple example to test concurrency/Process abstraction
//! 
//! Based on an Erlang example:
//! -module(ping).
//! 
//! -export([ping/1, pong/0, spawn_consumer/0, spawn_producer/1, start/0]).
//! 
//! ping(Pid) ->
//!     Pid ! {ping, self()},
//!     receive
//!         pong ->
//!             io:format("Received pong!!!~n"),
//!             ping(Pid)
//!     end.
//! 
//! pong() ->
//!     receive
//!         {ping, Pid} ->
//!             io:format("Received ping!!~n"),
//!             Pid ! pong,
//!             pong();
//!         die ->
//!             ok
//!         end.
//! 
//! spawn_consumer() ->
//!     spawn(ping, pong, []).
//! 
//! spawn_producer(Pid) ->
//!     spawn(ping, ping, [Pid]).
//! 
//! start() ->
//!     Pid = spawn_consumer(),
//!     spawn_producer(Pid).

mod consumer;
mod messages;
mod producer;

use std::{thread, time::Duration};

use consumer::Consumer;
use spawned_rt as rt;
use producer::Producer;

fn main() {
    rt::run(async {
        let consumer = Consumer::spawn_new().await;

        Producer::spawn_new(consumer.sender()).await;

        // giving it some time before ending
        thread::sleep(Duration::from_millis(1));
    })
}
