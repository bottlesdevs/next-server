# Bottles Next Server
The source code of the Next server.

## Usage
Call the server using [grpcurl](https://github.com/fullstorydev/grpcurl) or any gRPC client.

```bash
grpcurl -plaintext -proto ./proto/bottles.proto -d '{}' '[::1]:50052' bottles.Client.Health
{
  "ok": true
}
```
