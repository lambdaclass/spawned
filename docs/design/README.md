# Design Documents

Architectural decision records from the v0.5 API redesign. Approach B was chosen.

- **[FRAMEWORK_COMPARISON.md](FRAMEWORK_COMPARISON.md)** — Survey of 12+ actor frameworks (Actix, Ractor, Erlang, Akka, Orleans, Pony, Lunatic, Bastion, Proto.Actor, Trio, Loom, Ray, Swift, CAF, Dapr, Go CSP, core.async). Feature matrices, adopt/skip decisions.
- **[API_REDESIGN.md](API_REDESIGN.md)** — Initial plan: Handler<M>, Recipient<M>, object-safe Receiver pattern, supervision sketches.
- **[API_ALTERNATIVES_SUMMARY.md](API_ALTERNATIVES_SUMMARY.md)** — Full comparison of 6 API approaches (A-F) using the same chat room example. Includes analysis tables and comparison matrix.
- **[API_ALTERNATIVES_QUICK_REFERENCE.md](API_ALTERNATIVES_QUICK_REFERENCE.md)** — Condensed version of the above with code-first examples.
