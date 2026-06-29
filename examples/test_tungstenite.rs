use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use futures_util::{SinkExt, StreamExt};

#[tokio::main]
async fn main() {
    let url = "wss://zclaw.home.chekolyn.com/acp";
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert("Sec-WebSocket-Protocol", "zeroclaw.acp.v1".parse().unwrap());
    
    match connect_async(request).await {
        Ok((ws_stream, response)) => {
            println!("Connected! Status: {}", response.status());
            let (mut write, mut read) = ws_stream.split();
            write.send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#.into()
            )).await.unwrap();
            
            if let Some(msg) = read.next().await {
                println!("Response: {:?}", msg);
            }
        }
        Err(e) => println!("Error: {:?}", e),
    }
}
