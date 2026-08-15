# Bottles Next Server
A gRPC server implementing the `bottles.profiles.v1`, `bottles.store.v1`, and
`bottles.library.v1` protocols defined in [`next-proto`](https://github.com/bottlesdevs/next-proto).
It owns profile storage, credential custody, and dispatch to per-storefront
plugins so that any Bottles Next client (UI, CLI, or another service) can
manage storefront accounts through one consistent API.

## What it does

`next-profiles` is the runtime home for the profile/account system: a
**profile** is a named grouping of storefront sessions (Epic, GOG, Amazon,
...). Switching the active profile swaps the whole set of
storefront credentials at once. See the [`bottles.profiles.v1`](https://github.com/bottlesdevs/next-proto)
proto for the full message/RPC contract.

## Usage
Call the server using [grpcurl](https://github.com/fullstorydev/grpcurl) or any gRPC client.

## Running

```bash
cargo run
```

By default this starts a gRPC server exposing:

- `bottles.profiles.v1.Profile` — profile CRUD, activation, Steam-linked auto-switching
- `bottles.store.v1.Store` — storefront login/session lifecycle
- `bottles.library.v1.Library` — per-profile owned-games listing

With `tonic-reflection` enabled, you can introspect and call the server
without a local copy of the `.proto` files:

```bash
grpcurl -plaintext localhost:50051 list

grpcurl -plaintext localhost:50051 bottles.profiles.v1.Profile/ListProfiles

grpcurl -plaintext -d '{
  "profileId": "test",
  "storefronts": ["STOREFRONT_EPIC_GAMES"]
}' localhost:50051 bottles.library.v1.Library/ListGames
```

## Architecture

```
gRPC clients (Bottles UI, CLI, other Next services)
        │  bottles.profiles.v1 / store.v1 / library.v1
        ▼
next-profiles (this repo)
   ├── ProfileService impl   — Profile store, no storefront-specific logic
   ├── StoreService impl     — dispatches BeginLogin/CompleteLogin/RefreshSession
   │                            to the storefront's plugin by Storefront enum value
   ├── LibraryService impl   — fan-out ListGames/WatchGames across linked storefronts
   └── StorePlugin impls
         └── EpicPlugin      — wraps `egs-api`, owns Epic OAuth flow & session refresh
             (GogPlugin / AmazonPlugin: not yet implemented)
```

Session material (access/refresh tokens) is never sent over gRPC and is not
held in the proto messages returned to clients — only `AuthState` and display
metadata (`LinkedAccount`) go over the wire. Real session bytes are read and
written through `keyring`, scoped per profile/storefront, and only the owning
plugin (e.g. `EpicPlugin`) ever deserializes them.

## Storefront support status

| Storefront   | Status                                                                                        |
| ------------ | --------------------------------------------------------------------------------------------- |
| Epic Games   | via `egs-api`                                                                                 |
| Steam        | observed only (never authenticated by this server — see `SteamLink` in `bottles.profiles.v1`) |
| GOG          | not yet implemented                                                                           |
| Amazon Games | not yet implemented                                                                           |
