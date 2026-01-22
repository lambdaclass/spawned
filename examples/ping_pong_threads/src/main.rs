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
use producer::Producer;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        let consumer = Consumer::spawn_new();

        Producer::spawn_new(consumer.sender());

        // giving it some time before ending
        thread::sleep(Duration::from_millis(1));
    })
}
