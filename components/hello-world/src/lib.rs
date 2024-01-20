wit_bindgen::generate!({
    world:"hello",
    exports: {
        "wasi:http/incoming-handler": HelloWorld,
    }
});

use exports::wasi::http::incoming_handler::{Guest, IncomingRequest, ResponseOutparam};
use wasi::http::types::{Fields, OutgoingBody, OutgoingResponse};

struct HelloWorld;

impl Guest for HelloWorld {
    /// Handle a HTTP request
    fn handle(_request: IncomingRequest, response_out: ResponseOutparam) {
        let headers = Fields::new();
        let response = OutgoingResponse::new(headers);
        let body = response.body().unwrap();

        let out_stream = body.write().unwrap();

        out_stream
            .blocking_write_and_flush("Hello World!".as_bytes())
            .unwrap();

        OutgoingBody::finish(body, None).unwrap();
        ResponseOutparam::set(response_out, Ok(response));
    }
}
