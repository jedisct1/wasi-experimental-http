# `wasi-experimental-http-wasmtime`

![Crates.io](https://img.shields.io/crates/v/wasi-experimental-http-wasmtime)

Experimental HTTP library for WebAssembly in Wasmtime

### Adding support to a Wasmtime runtime

The easiest way to add support is by using the
[Wasmtime linker](https://docs.rs/wasmtime/0.25.0/wasmtime/struct.Linker.html):

```rust
let store = Store::default();
let mut linker = Linker::new(&store);
let wasi = Wasi::new(&store, ctx);

// link the WASI core functions
wasi.add_to_linker(&mut linker)?;

// link the experimental HTTP support
let http = Http::new(None);
http.add_to_linker(&mut linker)?;
```

The Wasmtime implementation also enables allowed domains - an optional and
configurable list of domains or hosts that guest modules are allowed to send
requests to. If `None` or an empty vector is passed, guest modules are **NOT**
allowed to make HTTP requests to any server. (Note that the hosts passed MUST
have the protocol also specified - i.e. `https://my-domain.com`, or
`http://192.168.0.1`, and if making requests to a subdomain, the subdomain MUST
be in the allowed list. See the the library tests for more examples).
