
# Docker Image for Moonlight-Web

1. Download [this folder](https://download-directory.github.io/?url=https%3A%2F%2Fgithub.com%2FMrCreativ3001%2Fmoonlight-web-stream%2Ftree%2Fmaster%2Fdocker)

2. Edit the [default-config.json](default-config.json) (or the config.json inside the moonlight-server volume)

- Change the [Nat 1 to 1 ips](https://github.com/MrCreativ3001/moonlight-web-stream?tab=readme-ov-file#webrtc-nat-1-to-1-ips) to the ip address of the device where you want to run the image on (this is required because docker is normally not able to see the ip of the device)

```json
{
    "webrtc_nat_1to1": {
        "ice_candidate_type": "host",
        "ips": [
            "127.0.0.1"
        ]
    }
}
```

3. Build the Docker image with:
```bash
docker build -t moonlight-web:v1.6 .
```

4. Run with
```bash
docker run -d -p 8080:8080 -p 40000-40100:40000-40100/udp --name moonlight-web moonlight-web:v1.6
```
or
```bash
docker run -d --net=host --name moonlight-web moonlight-web:v1.6
```

# Running with Turn Server

1. Set new Turn credentials in the [.env file](.env)
```dotenv
TURN_USER=myrandomuser
TURN_PASS=myrandompass
```

2. Edit the [default-config.json](default-config.json) (or the config.json inside the moonlight-server volume)

- Include the TURN server URL and credentials
- Change the [Nat 1 to 1 ips](https://github.com/MrCreativ3001/moonlight-web-stream?tab=readme-ov-file#webrtc-nat-1-to-1-ips) to the ip address of the device where you want to run the image on (this is required because docker is normally not able to see the ip of the device)

```json
{
  "webrtc_ice_servers": [
    {
      "urls": [
        "stun:l.google.com:19302",
        "stun:stun.l.google.com:19302",
        "stun:stun1.l.google.com:19302",
        "stun:stun2.l.google.com:19302",
        "stun:stun3.l.google.com:19302",
        "stun:stun4.l.google.com:19302"
      ],
      "username": "",
      "credential": ""
    },
    {
      "urls": [
        "turn:YOUR_PUBLIC_IP:3478?transport=udp",
        "turn:YOUR_PUBLIC_IP:3478?transport=tcp",
        "turn:YOUR_PUBLIC_IP:5349?transport=tcp",
        "turn:YOUR_PUBLIC_IP:443?transport=tcp"
      ],
      "username": "myrandomuser",
      "credential": "myrandompass"
    }
  ],
  "webrtc_nat_1to1": {
      "ice_candidate_type": "host",
      "ips": [
          "127.0.0.1"
      ]
  }
}
```

3. Start with docker compose
```bash
docker compose -f docker-compose.with-turn.yaml up -d
```