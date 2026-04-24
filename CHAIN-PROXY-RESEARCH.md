# Chain Proxy / Double-Hop VPN Research Report

> Research conducted: 2026-04-16
> Goal: Route traffic from Russia through RU entry server (51.250.73.78, Yandex Cloud) to LT exit server (94.131.13.243, Lithuania)

---

## 1. Executive Summary

Three proven approaches exist for chain/relay proxy with Xray, ranked by reliability:

| Approach                  | RU→LT Protocol                               | Complexity | Performance             | Best For                                    |
| ------------------------- | -------------------------------------------- | ---------- | ----------------------- | ------------------------------------------- |
| **A. L4 iptables relay**  | Transparent (passes through client's VLESS)  | Trivial    | Best (no re-encryption) | Simple relay, no routing logic              |
| **B. HAProxy L4 relay**   | Transparent + health checks + load balancing | Low        | Best                    | Multiple exit servers                       |
| **C. Xray-to-Xray chain** | VLESS+xHTTP+Reality (RU→LT)                  | Medium     | Good (~10-15% overhead) | Smart routing (YouTube via RU, rest via LT) |

**Recommended for your setup: Approach A (iptables) for simplicity, or C (Xray chain) if you want smart routing.**

---

## 2. Critical Finding: Yandex Cloud Is NOT a Reliable Whitelist Bypass

Per feedback on [Habr article 1021160](https://habr.com/ru/articles/1021160/) and [Sergei-thinker/vpn-setup](https://github.com/Sergei-thinker/vpn-setup) (updated 2026-04-17):

> **AS `Yandex.Cloud LLC` and AS `YANDEX LLC` are DIFFERENT autonomous systems. TSPU filters them separately. YC VMs are already blocked during active whitelists in some regions. YC as guaranteed bypass does NOT work.**

Your entry server 51.250.73.78 is in Yandex Cloud. This means:

- It will likely work for **wired/home ISP** traffic (TSPU shaping is weaker on datacenter ISPs)
- It will likely **NOT** bypass mobile operator whitelists (MTS, Megafon, Beeline, Tele2)
- For mobile whitelist bypass, you'd need VPS from providers whose IPs fall within whitelisted ranges (VK Cloud, sometimes Timeweb/VDSina/Selectel)

**However**, for wired ISPs, a RU VPS (including Yandex Cloud) typically bypasses TSPU throttling because:

> "Algorithms for shaping are not yet deployed on all TSPU boxes, or are intentionally not deployed at full power on hosting/datacenter ISPs" — [xcvtt/miniature-octo-palm-tree](https://github.com/xcvtt/miniature-octo-palm-tree)

---

## 3. TSPU Blocking Status (April 2026)

### What's blocked

| Protocol                         | Status                         | Source                                                             |
| -------------------------------- | ------------------------------ | ------------------------------------------------------------------ |
| OpenVPN                          | Fully blocked                  | Signature detection                                                |
| WireGuard                        | Fully blocked                  | UDP structure detection                                            |
| VLESS+Reality+Vision on port 443 | Partially blocked on home ISPs | [net4people/bbs#546](https://github.com/net4people/bbs/issues/546) |
| Shadowsocks (old)                | Partially blocked              | Pattern detection                                                  |
| 469+ commercial VPNs             | Blocked                        | IP/signature                                                       |

### What works

| Protocol                                | Status             | Notes                                                                         |
| --------------------------------------- | ------------------ | ----------------------------------------------------------------------------- |
| VLESS+Reality+xHTTP                     | Working            | Best current option                                                           |
| VLESS+Reality on non-443 ports          | Working            | Change to 22/23/8443 helps                                                    |
| VLESS+Reality+Vision without flow + mux | Working            | Remove `xtls-rprx-vision`, add mux                                            |
| Shadowsocks 2022                        | Working (unstable) | Could be blocked at any time                                                  |
| VLESS+WS+TLS (with valid cert)          | Working            | ~10 Mbps in Russia per [#5383](https://github.com/XTLS/Xray-core/issues/5383) |

### TLS Connection Policing (Nov 2025+)

Per [net4people/bbs#546](https://github.com/net4people/bbs/issues/546):

- Some home ISPs (MTS/MGTS Moscow, JustLan, LanInterCom) test TLS-based policing
- Connections drop when actual data flows through tunnel
- Traffic-volume based, not bandwidth: more TLS connections open = faster disconnection
- After ~60 seconds of inactivity, blocking resets
- **Workarounds**: non-443 ports, remove flow + add mux, use xHTTP/H2

---

## 4. Approach A: L4 iptables Relay (Simplest)

The RU server is a "dumb" packet forwarder. No Xray needed on RU server. Client connects to RU IP but the VLESS handshake goes directly to LT server.

### Architecture

```
Client ──(VLESS+Reality)──> RU Server (iptables DNAT) ──(raw packets)──> LT Server ──> Internet
```

### RU Entry Server Config (51.250.73.78)

```bash
#!/bin/bash
# No Xray needed — pure iptables forwarding

LT_IP="94.131.13.243"   # Lithuania exit server
TARGET_PORT="443"        # Port of VLESS inbound on LT server
LOCAL_PORT="443"         # Port clients connect to
SSH_PORT="22"

# Enable IP forwarding
sudo sysctl -w net.ipv4.ip_forward=1
echo 'net.ipv4.ip_forward = 1' | sudo tee /etc/sysctl.d/99-ipforward.conf
sudo sysctl -p /etc/sysctl.d/99-ipforward.conf

sudo apt install -y iptables iptables-persistent

# Reset rules
sudo iptables -P INPUT ACCEPT
sudo iptables -P FORWARD ACCEPT
sudo iptables -P OUTPUT ACCEPT
sudo iptables -F
sudo iptables -t nat -F

sudo iptables -A INPUT -i lo -j ACCEPT
sudo iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
sudo iptables -A INPUT -p tcp --dport "$SSH_PORT" -j ACCEPT

# DNAT: forward incoming traffic to Lithuania
# TCP
sudo iptables -t nat -A PREROUTING -p tcp --dport "$LOCAL_PORT" -j DNAT --to-destination "$LT_IP:$TARGET_PORT"
# UDP (for QUIC)
sudo iptables -t nat -A PREROUTING -p udp --dport "$LOCAL_PORT" -j DNAT --to-destination "$LT_IP:$TARGET_PORT"

# SNAT: masquerade so LT server sees RU server's IP as source
sudo iptables -t nat -A POSTROUTING -j MASQUERADE

# Drop everything else
sudo iptables -A INPUT -j DROP

# Save
sudo netfilter-persistent save
```

### LT Exit Server Config

No changes needed — keep existing Xray VLESS+Reality inbound as-is.

### Client Config

Same as direct connection to LT, but replace LT IP with RU IP (51.250.73.78).

### Pros/Cons

- **Pro**: Zero overhead, maximum speed, minimal RU server resources
- **Pro**: RU server sees no decrypted traffic, just encrypted packets
- **Con**: No smart routing (can't send YouTube through RU directly)
- **Con**: No health checks (if LT dies, everything dies)
- **Con**: TSPU can still see the connection to the foreign ASN (94.131.13.243) from the RU server — but datacenter-to-datacenter traffic is less scrutinized

---

## 5. Approach B: HAProxy L4 Relay (With Health Checks)

Same as iptables but with health checks and optional load balancing.

### RU Entry Server Config (51.250.73.78)

```bash
sudo apt install -y haproxy

cat > /etc/haproxy/haproxy.cfg << 'EOF'
global
    log /dev/log local0

defaults
    log     global
    mode    tcp
    option  tcplog
    option  dontlognull
    timeout connect 5000
    timeout client  50000
    timeout server  50000

frontend vless-in
    bind *:443
    default_backend lt-exit

backend lt-exit
    mode tcp
    server lt_vps 94.131.13.243:443 check inter 10s fall 3 rise 2
    # Add more exit servers for failover:
    # server de_vps 1.2.3.4:443 check inter 10s fall 3 rise 2 backup
EOF

sudo systemctl restart haproxy
sudo systemctl enable haproxy
```

---

## 6. Approach C: Xray-to-Xray Chain (Smart Routing)

Two independent Xray instances. Client→RU uses one protocol, RU→LT uses another. RU server can make routing decisions (e.g., YouTube direct from RU, everything else via LT).

### Architecture

```
Client ──(VLESS+Reality, TCP)──> RU Xray ──(VLESS+xHTTP+Reality)──> LT Xray ──> Internet
         SNI: gosuslugi.ru                  SNI: microsoft.com
```

### Protocol between RU→LT: VLESS+xHTTP+Reality

**Why xHTTP?** Per [xcvtt guide](https://github.com/xcvtt/miniature-octo-palm-tree):

- xHTTP is the newest transport, optimized for censorship resistance
- It works as multiplexed HTTP POST/GET — looks like normal web browsing
- Combined with Reality, the RU→LT connection looks like HTTPS to microsoft.com
- Better performance than gRPC, more stable than WebSocket

### LT Exit Server Config (94.131.13.243)

Add a **separate** inbound for relay traffic (keep existing inbounds for direct clients):

On the LT server (using 3X-UI or manual config), create a new inbound:

| Setting           | Value                   |
| ----------------- | ----------------------- |
| Remark            | `relay-xhttp`           |
| Protocol          | `vless`                 |
| Port              | **10443**               |
| Transmission      | **xHTTP**               |
| xHTTP Mode        | `auto`                  |
| Security          | **Reality**             |
| Target (dest)     | `www.microsoft.com:443` |
| SNI (serverNames) | `www.microsoft.com`     |

Generate keys: `xray x25519`

Record: UUID, Public Key, Short ID for the RU server config.

Open firewall: `ufw allow 10443`

**Manual Xray config for the relay inbound** (add to existing config):

```json
{
  "tag": "inbound-relay-xhttp",
  "listen": "0.0.0.0",
  "port": 10443,
  "protocol": "vless",
  "settings": {
    "clients": [
      {
        "id": "<RELAY_UUID>",
        "flow": ""
      }
    ],
    "decryption": "none"
  },
  "streamSettings": {
    "network": "xhttp",
    "xhttpSettings": {
      "path": "/relay-secret-path"
    },
    "security": "reality",
    "realitySettings": {
      "show": false,
      "dest": "www.microsoft.com:443",
      "xver": 0,
      "serverNames": ["www.microsoft.com", "microsoft.com"],
      "privateKey": "<LT_RELAY_PRIVATE_KEY>",
      "shortIds": ["<LT_RELAY_SHORT_ID>"]
    }
  },
  "sniffing": {
    "enabled": true,
    "destOverride": ["http", "tls", "quic"]
  }
}
```

### RU Entry Server Config (51.250.73.78) — Full Xray Config

```json
{
  "log": {
    "loglevel": "warning",
    "access": "/var/log/xray/access.log",
    "error": "/var/log/xray/error.log"
  },
  "dns": {
    "servers": [
      "https+local://1.1.1.1/dns-query",
      "https+local://8.8.8.8/dns-query",
      "localhost"
    ]
  },
  "inbounds": [
    {
      "tag": "relay-inbound",
      "listen": "0.0.0.0",
      "port": 443,
      "protocol": "vless",
      "settings": {
        "clients": [
          {
            "id": "<RU_RELAY_UUID>",
            "flow": ""
          }
        ],
        "decryption": "none"
      },
      "streamSettings": {
        "network": "tcp",
        "security": "reality",
        "realitySettings": {
          "show": false,
          "dest": "www.gosuslugi.ru:443",
          "xver": 0,
          "serverNames": ["www.gosuslugi.ru", "gosuslugi.ru"],
          "privateKey": "<RU_PRIVATE_KEY>",
          "shortIds": ["<RU_SHORT_ID>"]
        },
        "tcpSettings": {
          "acceptProxyProtocol": false,
          "header": { "type": "none" }
        }
      },
      "sniffing": {
        "enabled": true,
        "destOverride": ["http", "tls", "quic"]
      }
    }
  ],
  "outbounds": [
    {
      "tag": "relay-to-lt",
      "protocol": "vless",
      "settings": {
        "vnext": [
          {
            "address": "94.131.13.243",
            "port": 10443,
            "users": [
              {
                "id": "<RELAY_UUID>",
                "flow": "",
                "encryption": "none"
              }
            ]
          }
        ]
      },
      "streamSettings": {
        "network": "xhttp",
        "security": "reality",
        "realitySettings": {
          "show": false,
          "fingerprint": "chrome",
          "serverName": "www.microsoft.com",
          "publicKey": "<LT_RELAY_PUBLIC_KEY>",
          "shortId": "<LT_RELAY_SHORT_ID>"
        },
        "xhttpSettings": {
          "path": "/relay-secret-path"
        }
      }
    },
    {
      "protocol": "freedom",
      "tag": "direct"
    },
    {
      "protocol": "blackhole",
      "tag": "block"
    }
  ],
  "routing": {
    "domainStrategy": "AsIs",
    "rules": [
      {
        "type": "field",
        "domain": ["geosite:youtube", "geosite:category-ru"],
        "outboundTag": "direct"
      },
      {
        "type": "field",
        "outboundTag": "relay-to-lt"
      }
    ]
  }
}
```

### Client Config

| Field       | Value                         |
| ----------- | ----------------------------- |
| Protocol    | VLESS                         |
| Address     | 51.250.73.78 (RU entry)       |
| Port        | 443                           |
| UUID        | `<RU_RELAY_UUID>`             |
| Transport   | TCP                           |
| Security    | Reality                       |
| SNI         | www.gosuslugi.ru              |
| Fingerprint | chrome                        |
| Public Key  | `<RU_PUBLIC_KEY>`             |
| Short ID    | `<RU_SHORT_ID>`               |
| Flow        | (empty — no xtls-rprx-vision) |

---

## 7. Xray's Built-in Mechanisms for Chaining

### 7.1 proxySettings (Outbound Proxy Chain)

Per [official docs](https://xtls.github.io/en/config/outbound.html):

```json
{
  "outbounds": [
    {
      "tag": "final-exit",
      "protocol": "vless",
      "settings": { ... },
      "proxySettings": {
        "tag": "first-hop",
        "transportLayer": true
      }
    },
    {
      "tag": "first-hop",
      "protocol": "vless",
      "settings": { ... }
    }
  ]
}
```

- `proxySettings.tag` — routes this outbound's traffic through another outbound first
- `transportLayer: true` — enables underlying transport (Reality/xHTTP) to work through the chain
- **Without `transportLayer: true`**, streamSettings of the chained outbound are ignored
- **Conflicts with** `SockOpt.dialerProxy` — use one or the other
- This is the **client-side** chain mechanism (both hops configured on the same Xray instance)

### 7.2 Reverse Proxy (bridge/portal)

Per [official docs](https://xtls.github.io/en/config/reverse.html):

- **Purpose**: Expose services behind NAT, NOT for chain proxy
- `bridge` = host behind NAT, actively connects to `portal`
- `portal` = public-facing host, receives requests and forwards to bridge
- **Not suitable** for your use case (it's for reverse tunneling, not forward chaining)

### 7.3 Server-side Inbound→Outbound Routing (Approach C)

- The relay server receives traffic on an inbound, makes routing decisions, forwards to an outbound
- **This is the recommended approach** for double-hop with smart routing
- Each server is independent, no special "bridge mode" needed

---

## 8. Real-World Success/Failure Reports

### Working: Moscow→Netherlands VLESS+WS+TLS relay chain

[XTLS/Xray-core#5383](https://github.com/XTLS/Xray-core/issues/5383) (Dec 2025):

- User in Russia, Moscow server → Netherlands server
- **PC works perfectly** via relay chain
- Mobile devices: mixed results (OPPO fails, POCO works)
- Protocol: VLESS+WS+TLS with Let's Encrypt
- **VLESS+REALITY nearly non-functional** in their region
- VLESS+WS+TLS provides ~10 Mbps

### Working: Whitelist bypass via RU VPS relay

[kort0881/russia-whitelist#21](https://github.com/kort0881/russia-whitelist/discussions/21) (Dec 2025, 94 comments, 432 replies):

- Massive discussion about VK Cloud VPS for whitelist bypass
- iptables DNAT relay is the most popular approach
- Users confirm it works for mobile operator whitelists
- Key requirement: RU VPS IP must be in the whitelisted CIDR range

### Working: 4-layer VPN architecture

[Sergei-thinker/vpn-setup](https://github.com/Sergei-thinker/vpn-setup) (April 2026, 47 stars):

- Layer 0: Direct VLESS Reality
- Layer 1: RU Relay VPS (VLESS Reality → VLESS xHTTP)
- Layer 2: WebRTC (emergency)
- Layer 3: Cloudflare CDN (backup for WiFi)
- Automated deployment scripts for both relay and exit server
- **Confirmed working** as of April 2026

### Failure: Shadowsocks inbound → VLESS+gRPC outbound

[XTLS/Xray-core#5428](https://github.com/XTLS/Xray-core/issues/5428) (Dec 2025):

- User tried SS2022 inbound on Server 1 → VLESS+gRPC outbound to Server 2
- Each server works independently but **chain fails**
- Connection times out or rejected
- **Lesson**: Don't mix protocols carelessly; use same protocol or simple freedom outbound

### Failure: dialerProxy with Reality (Linux bug)

[XTLS/Xray-core#1844](https://github.com/XTLS/Xray-core/issues/1844) (Mar 2023):

- Chain: Reality outbound → Trojan outbound using `dialerProxy`
- Works on Windows, fails on Linux with `HTTP/0.9 when not allowed`
- Old bug, may be fixed in current Xray versions

---

## 9. Performance Impact of Double Hop

Based on real-world reports:

- **L4 relay (iptables/HAProxy)**: Negligible overhead (<1ms latency added, no throughput loss)
- **Xray chain (Approach C)**: ~10-15% throughput reduction due to double encryption/decryption
- **Latency**: RU datacenter → LT datacenter adds ~20-40ms
- **Practical speed**: Users report 10-50 Mbps through chain setups in Russia (depends on ISP throttling)

---

## 10. Projects That Automate This Setup

| Project                            | Stars | What It Does                                        | Link                                                                    |
| ---------------------------------- | ----- | --------------------------------------------------- | ----------------------------------------------------------------------- |
| **Sergei-thinker/vpn-setup**       | 47    | Full 4-layer VPN with automated relay deploy        | [GitHub](https://github.com/Sergei-thinker/vpn-setup)                   |
| **xcvtt/miniature-octo-palm-tree** | 181   | Comprehensive 5-level bypass guide (2026)           | [GitHub](https://github.com/xcvtt/miniature-octo-palm-tree)             |
| **kort0881/russia-whitelist**      | 251   | Whitelist analysis + VK Cloud relay discussion      | [GitHub](https://github.com/kort0881/russia-whitelist)                  |
| **MHSanaei/3x-ui**                 | 20k+  | Xray panel with outbound/routing support for chains | [GitHub](https://github.com/MHSanaei/3x-ui)                             |
| **rz6agx gist**                    | 7     | Multi-level Xray proxy scheme with configs          | [Gist](https://gist.github.com/rz6agx/7ff6a6ada0ccc1613b38b50f81749e78) |

---

## 11. Specific Recommendations for Your Setup

### Your servers:

- Entry: 51.250.73.78 (Yandex Cloud, Russia)
- Exit: 94.131.13.243 (Lithuania, ASN blocked by TSPU)

### Recommended plan:

**Phase 1: Start with iptables relay (5 minutes)**

1. SSH to RU server (51.250.73.78)
2. Run the iptables script from Section 4
3. Client connects to 51.250.73.78 instead of 94.131.13.243
4. Test — this alone might solve the blocking for wired ISPs

**Phase 2: If Phase 1 works but you want smart routing**

1. Install Xray on RU server
2. Create VLESS+Reality inbound (SNI: gosuslugi.ru or another RU government site)
3. Add VLESS+xHTTP+Reality outbound to LT server
4. Add new xHTTP inbound on port 10443 on LT server
5. Configure routing: YouTube/RU sites → direct, everything else → LT

**Phase 3: If mobile whitelists are a problem**

- Yandex Cloud may not be whitelisted — test first
- If blocked, consider VK Cloud or Timeweb VPS as relay
- Use `https://hyperion-cs.github.io/dpi-checkers/ru/tcp-16-20/` to test your IP

### Important notes:

- **Don't use `flow: xtls-rprx-vision`** on the relay inbound — it causes issues with chaining
- **Use xHTTP** for RU→LT hop (not WS, not gRPC) — most resilient in 2026
- **Consider non-443 ports** (22, 8443, 10443) — port 443 is most scrutinized
- **Enable BBR** on both servers for better performance over lossy connections
- **SNI choices**: gosuslugi.ru for RU server inbound, microsoft.com for LT server inbound

---

## Sources

### GitHub Issues & Discussions

- [net4people/bbs#546](https://github.com/net4people/bbs/issues/546) — Russia TLS connection policing (68 comments)
- [net4people/bbs#490](https://github.com/net4people/bbs/issues/490) — Russia new blocking method (79 comments)
- [net4people/bbs#363](https://github.com/net4people/bbs/issues/363) — Russia SS/VMess blocking
- [XTLS/Xray-core#5383](https://github.com/XTLS/Xray-core/issues/5383) — Multi-hop relay chain issue
- [XTLS/Xray-core#5428](https://github.com/XTLS/Xray-core/issues/5428) — SS→VLESS relay failure
- [XTLS/Xray-core#1844](https://github.com/XTLS/Xray-core/issues/1844) — dialerProxy chain issue
- [XTLS/Xray-core#518](https://github.com/XTLS/Xray-core/issues/518) — Relay chains feature request
- [XTLS/Xray-core#5579](https://github.com/XTLS/Xray-core/discussions/5579) — dialerProxy + whitelist bypass
- [XTLS/Xray-core#4645](https://github.com/XTLS/Xray-core/discussions/4645) — Chain Xrays for VPN routing
- [kort0881/russia-whitelist#21](https://github.com/kort0881/russia-whitelist/discussions/21) — VK Cloud whitelist bypass (432 replies)

### GitHub Projects

- [Sergei-thinker/vpn-setup](https://github.com/Sergei-thinker/vpn-setup) — Automated 4-layer VPN with relay scripts
- [xcvtt/miniature-octo-palm-tree](https://github.com/xcvtt/miniature-octo-palm-tree) — 2026 bypass methods guide
- [rz6agx/Xray-proxy-scheme](https://gist.github.com/rz6agx/7ff6a6ada0ccc1613b38b50f81749e78) — Multi-level proxy scheme
- [XTLS/Xray-examples](https://github.com/XTLS/Xray-examples) — Official examples (ReverseProxy/)

### Official Documentation

- [Xray Outbound Proxy (proxySettings)](https://xtls.github.io/en/config/outbound.html) — Chain proxy settings
- [Xray Reverse Proxy](https://xtls.github.io/en/config/reverse.html) — Bridge/portal (NOT for chain proxy)
- [Xray Transport (dialerProxy)](https://xtls.github.io/en/config/transport.html) — SockOpt.dialerProxy
