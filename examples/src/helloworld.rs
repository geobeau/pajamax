use pajamax::status::Status;

use helloworld::{Greeter, GreeterServer};
use helloworld::{HelloReply, HelloRequest};

mod helloworld {
    pajamax::include_proto!("helloworld");
}

struct MyGreeter();

#[async_trait::async_trait(?Send)]
impl Greeter for MyGreeter {
    async fn say_hello(&self, req: HelloRequest) -> Result<HelloReply, Status> {
        let reply = HelloReply {
            message: format!("Hello {}!", req.name),
        };
        Ok(reply)
    }
}

fn main() {
    let addr = "127.0.0.1:50051";

    println!("GreeterServer listening on {}", addr);

    pajamax::Config::new()
    .num_cores(8)
        .max_concurrent_connections(100000)
        .add_service(|| GreeterServer::new(MyGreeter()))
        .serve(addr)
        .unwrap();
}
