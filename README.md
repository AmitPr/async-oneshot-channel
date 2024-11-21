# async-oneshot-channel

A simple (<100 lines of logic) "oneshot" channel for asynchronously sending a single value between tasks, in a thread-safe manner. This implementation supports cloned senders while ensuring only one send operation will succeed.

## Usage

```rust
use futures::executor::block_on;

// Create a new channel
let (tx, rx) = oneshot();

// Send a value
tx.send(42).unwrap();

// Receive the value asynchronously
let result = block_on(rx.recv());
assert_eq!(result, Some(42));
```

## Features

- Multiple senders (through cloning) with guaranteed single-use semantics
- Async support for receiver, instant send.
- Zero unsafe code in the public API, one unsafe line enforcing `MaybeUninit` safety.
- Thread-safe: implements `Send` and `Sync` where appropriate
- Cancellation support: receivers get `None` if all senders drop

Licensed under the MIT license.
