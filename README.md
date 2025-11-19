## yt-dlp UI

Simple web front-end for managing yt-dlp playlists, archives, and downloaded albums.

### Development

```bash
cargo run
```

The server listens on `BIND_ADDR` (default `0.0.0.0:8090`). Files, archives, and per-user settings live under `DATA_DIR` (default `./data` when running locally).

### Linux build & run

1. Install system dependencies: `sudo apt install build-essential pkg-config libssl-dev` (adjust the package manager for your distro) plus [Rustup](https://rustup.rs/) for the Rust toolchain.
2. Clone the repository and build it in release mode:
   ```bash
   git clone https://github.com/stdakov/yt-dlp-ui.git
   cd yt-dlp-ui
   cargo build --release
   ```
   The binary lands under `target/release/yt-dlp-ui`.
3. Create a data directory and launch the server:
   ```bash
   mkdir -p ~/yt-dlp-data
   DATA_DIR=~/yt-dlp-data BIND_ADDR=0.0.0.0:8090 ./target/release/yt-dlp-ui
   ```
   Open `http://localhost:8090` in your browser, and adjust download/archive folders from **Settings → Storage folders** as needed.

### Docker

Build the image:

```bash
docker build -t yt-dlp-ui .
```

Run it with host directories mounted wherever you keep music and archive files:

```bash
docker run --rm -p 8090:8090 \
  -v /mnt/music:/downloads \
  -v /mnt/archives:/archives \
  -e DOWNLOADS_DIR=/downloads \
  -e ARCHIVES_DIR=/archives \
  yt-dlp-ui
```

Key environment variables (all optional, but handy when invoking `docker run`/`docker compose`):

| Variable        | Purpose                                     | Default in image  |
| --------------- | ------------------------------------------- | ----------------- |
| `DATA_DIR`      | Root for app state (`settings.json`)        | `/data`           |
| `DOWNLOADS_DIR` | Folder where yt-dlp saves media             | `/data/downloads` |
| `ARCHIVES_DIR`  | Folder that stores yt-dlp download archives | `/data/archives`  |
| `BIND_ADDR`     | Listen address/port                         | `0.0.0.0:8090`    |

Inside the UI, **Settings → Storage folders** lets you adjust the same directories at runtime. Any paths provided via environment variables override defaults immediately, so new containers can set their main download folder directly in the `docker run` command.

### Docker Compose

A ready-to-use [`docker-compose.yml`](docker-compose.yml) is included. It mounts the repository `./data` folder into `/data` inside the container and exposes port `8090`.

Build and start:

```bash
docker compose up --build
```

To run in the background use `docker compose up -d`, and stop via `docker compose down`. Adjust volumes or ports inside the compose file to match your storage layout.
