use std::fmt::Write;

pub fn generate(service: prost_build::Service, buf: &mut String) {
    gen_trait_dispatch(&service, buf);
    gen_trait_shard(&service, buf);
    gen_request_type(&service, buf);
    gen_server(&service, buf);
    gen_shard_server(&service, buf);
}

// trait {Service}Dispatch
//
// The `dispatch_to()` returns a &{Service}RequestTx to
// specify where to dispatch the requets.
//
// Applications should implement this trait for a server context.
// The server context is global, wrapped by `Arc` and shared by all
// network threads.
fn gen_trait_dispatch(service: &prost_build::Service, buf: &mut String) {
    writeln!(
        buf,
        "pub trait {}Dispatch {{
            fn dispatch_to(&self, req: &{}Request) -> &{}RequestTx;
        }}",
        service.name, service.name, service.name
    )
    .unwrap();
}

// trait {Service}Shard
//
// Defines all gRPC methods to make replies.
// These methods will be called in backend shard threads.
//
// Applications should implement this trait for a backend shard
// context struct. Each shard thread owns a context instence,
// so these methods take mutable reference of `self`.
fn gen_trait_shard(service: &prost_build::Service, buf: &mut String) {
    writeln!(buf, "pub trait {}Shard {{", service.name).unwrap();

    for m in service.methods.iter() {
        writeln!(
            buf,
            "fn {}(&mut self, request: {}) -> pajamax::Response<{}>;",
            m.name, m.input_type, m.output_type
        )
        .unwrap();
    }
    writeln!(buf, "}}").unwrap();
}

// enum ${Service}Request
//
// Used to dispatch requests through channel.
//
// Applications need not access this.
fn gen_request_type(service: &prost_build::Service, buf: &mut String) {
    // enum
    writeln!(buf, "#[derive(Debug, PartialEq)]").unwrap();
    writeln!(buf, "pub enum {}Request {{", service.name).unwrap();

    for m in service.methods.iter() {
        writeln!(buf, "{}({}),", m.proto_name, m.input_type).unwrap();
    }
    writeln!(buf, "}}").unwrap();

    // channel types
    writeln!(
        buf,
        "pub type {}RequestTx = pajamax::dispatch::RequestTx<{}Request>;
         pub type {}RequestRx = pajamax::dispatch::RequestRx<{}Request>;",
        service.name, service.name, service.name, service.name
    )
    .unwrap();
}

// struct ${Service}Server
//
// Intermediary between pajamax::PajamaxService and application's server.
// Applications should call ${Service}Server::new(AppServer) to make a
// Pajamax service.
fn gen_server(service: &prost_build::Service, buf: &mut String) {
    writeln!(
        buf,
        "pub struct {}Server<T: {}Dispatch>(T);

        #[allow(dead_code)]
        impl<T: {}Dispatch> {}Server<T> {{
            pub fn new(inner: T) -> Self {{ Self(inner) }}

            pub fn inner(&self) -> &T {{ &self.0 }}
            pub fn inner_mut(&mut self) -> &mut T {{ &mut self.0 }}
        }}",
        service.name, service.name, service.name, service.name
    )
    .unwrap();

    // impl pajamax::PajamaxService for ${Service}Server
    writeln!(
        buf,
        "impl<T> pajamax::PajamaxService for {}Server<T>
        where T: {}Dispatch
        {{
            fn is_dispatch_mode(&self) -> bool {{ true }}
        ",
        service.name, service.name
    )
    .unwrap();

    gen_service_route(service, buf);
    gen_service_handle(service, buf);

    writeln!(buf, "}}").unwrap();
}

// impl PajamaxService::route()
fn gen_service_route(service: &prost_build::Service, buf: &mut String) {
    writeln!(
        buf,
        "fn route(&self, path: &[u8]) -> Option<usize> {{
            match path {{"
    )
    .unwrap();

    for (i, m) in service.methods.iter().enumerate() {
        writeln!(
            buf,
            "b\"/{}.{}/{}\" => Some({}),",
            service.package, service.name, m.proto_name, i
        )
        .unwrap();
    }
    writeln!(buf, "_ => None, }} }}").unwrap();
}

// impl PajamaxService::handle()
fn gen_service_handle(service: &prost_build::Service, buf: &mut String) {
    writeln!(
        buf,
        "fn handle(
            &self,
            req_disc: usize,
            req_buf: &[u8],
            stream_id: u32,
        ) -> Result<(), pajamax::error::Error> {{
            use prost::Message;
            let request = match req_disc {{"
    )
    .unwrap();

    for (i, m) in service.methods.iter().enumerate() {
        writeln!(
            buf,
            "{} => {}Request::{}({}::decode(req_buf)?),",
            i, service.name, m.proto_name, m.input_type
        )
        .unwrap();
    }

    writeln!(
        buf,
        "       d => unreachable!(\"invalid req_disc: {{d}}\"),
            }};

            let req_tx = self.0.dispatch_to(&request);
            pajamax::dispatch::dispatch(req_tx, request, stream_id)
        }}"
    )
    .unwrap();
}

// struct {Service}ShardServer
//
// Applications should:
// 1. create some backend shard threads,
// 2. call {Service}ShardServer::new(AppShardServer) to make a server,
// 3. receive requests from channel,
// 4. call {Service}ShardServer::handle(request) to handle them.
fn gen_shard_server(service: &prost_build::Service, buf: &mut String) {
    writeln!(
        buf,
        "pub struct {}ShardServer<T: {}Shard>(T);

        impl<T: {}Shard> {}ShardServer<T> {{
            pub fn new(inner: T) -> Self {{ Self(inner) }}

            #[allow(dead_code)]
            pub fn inner(&self) -> &T {{ &self.0 }}

            #[allow(dead_code)]
            pub fn inner_mut(&mut self) -> &mut T {{ &mut self.0 }}

            // Handle the request and response.
            #[allow(dead_code)]
            pub fn handle(&mut self, disp_req: pajamax::dispatch::DispatchRequest<{}Request>) {{
                let response = self.handle_request(disp_req.request);
                disp_req.stream.response(response);
            }}

            // Handle the request only. The caller should response later.
            #[allow(dead_code)]
            pub fn handle_request(&mut self, request: {}Request) -> pajamax::Response<Box<dyn pajamax::ReplyEncode>>
            {{
                match request {{",
        service.name, service.name, service.name, service.name, service.name, service.name
    )
    .unwrap();

    // continue of `fn handle()`
    for m in service.methods.iter() {
        writeln!(
            buf,
            "{}Request::{}(request) => {{
                self.0.{}(request).map(|reply|
                    Box::new(reply) as Box<dyn pajamax::ReplyEncode>)
            }}",
            service.name, m.proto_name, m.name
        )
        .unwrap();
    }
    writeln!(buf, "}} }} }}").unwrap();
}
