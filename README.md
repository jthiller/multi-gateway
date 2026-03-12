# helium-multi-gateway
[![Continuous Integration](https://github.com/lthiery/multi-gateway/actions/workflows/hygiene.yml/badge.svg)](https://github.com/lthiery/multi-gateway/actions/workflows/hygiene.yml)

A multi-gateway aggregator for the Helium LoRaWAN network. It accepts connections from
multiple LoRaWAN gateways via the Semtech UDP protocol, manages per-gateway keypairs, and
routes packets to the Helium packet router over gRPC.

Each gateway must have a unique MAC ID, as this is used to pair the gateway to a
cryptographic keypair (created automatically on first connection). A single instance of this
application supports one LoRaWAN region at a time; to serve gateways in multiple regions,
run separate instances with different configurations.

## Installing from Debian package

Download the `.deb` from the
[CI artifacts](https://github.com/lthiery/multi-gateway/actions/workflows/integration.yml)
(each successful run uploads `helium-multi-gateway-deb`), then install it:

```bash
sudo dpkg -i helium-multi-gateway_*.deb
```

This will:

- install the binary to `/usr/bin/helium-multi-gateway`
- install a default config to `/etc/helium-multi-gateway/settings.toml`
- install a systemd unit and enable/start the service

Verify the service is running:

```bash
sudo systemctl status helium-multi-gateway
sudo journalctl -u helium-multi-gateway -f
```

## Configuration

The configuration file lives at `/etc/helium-multi-gateway/settings.toml`:

```toml
keys_dir = "/var/lib/helium-multi-gateway/keys"
region = "US915"
api_addr = "127.0.0.1:4468"

[gwmp]
addr = "0.0.0.0"
port = 1680

[router]
uri = "http://mainnet-router.helium.io:8080/"
queue = 100

[log]
level = "info"
```

### Changing the region

Edit the `region` field to match your deployment. Common values:

| Region  | Description          |
|---------|----------------------|
| `US915` | United States 915 MHz |
| `EU868` | Europe 868 MHz       |
| `AU915` | Australia 915 MHz    |
| `AS923_1` | Asia 923 MHz (group 1) |
| `KR920` | South Korea 920 MHz  |
| `IN865` | India 865 MHz        |

After editing, restart the service:

```bash
sudo systemctl restart helium-multi-gateway
```

## REST API

The API listens on `api_addr` (default `127.0.0.1:4468`).

### `GET /gateways`

List all known gateways.

```bash
curl http://127.0.0.1:4468/gateways
```

```json
{
  "gateways": [
    {
      "mac": "AABBCCDDEEFF0011",
      "public_key": "...",
      "connected": true,
      "connected_seconds": 3600,
      "last_uplink_seconds_ago": 5
    }
  ],
  "total": 1,
  "connected": 1
}
```

### `GET /gateways/{mac}`

Get status for a single gateway. The MAC is 16 hex characters.

```bash
curl http://127.0.0.1:4468/gateways/AABBCCDDEEFF0011
```

### `POST /gateways/{mac}/sign`

Sign arbitrary data with a gateway's keypair. The request and response bodies are
base64-encoded.

```bash
curl -X POST http://127.0.0.1:4468/gateways/AABBCCDDEEFF0011/sign \
  -H 'Content-Type: application/json' \
  -d '{"data": "<base64-encoded-data>"}'
```

```json
{
  "signature": "<base64-encoded-signature>"
}
```

### `GET /metrics`

Prometheus-formatted metrics.

```bash
curl http://127.0.0.1:4468/metrics
```

## CLI

Many of the REST endpoints above can be exercised through the CLI. The CLI connects to the
running server's API, so the service must be running.

```
helium-multi-gateway [OPTIONS] <COMMAND>
```

### `helium-multi-gateway gateways list`

Lists all gateways and their connection status (calls `GET /gateways` under the hood).

```bash
helium-multi-gateway -c /etc/helium-multi-gateway/settings.toml gateways list
```

### `helium-multi-gateway gateways sign`

Signs data with a gateway's keypair and displays the result as a QR code (default) or
base64 string (calls `POST /gateways/{mac}/sign`).

```bash
# Display signature as QR code
helium-multi-gateway gateways sign AABBCCDDEEFF0011 <base64-data>

# Output signature as base64
helium-multi-gateway gateways sign AABBCCDDEEFF0011 <base64-data> --format base64
```

### `helium-multi-gateway run`

Starts the server. This is what the systemd service invokes.

```bash
helium-multi-gateway --config settings.toml run
```
