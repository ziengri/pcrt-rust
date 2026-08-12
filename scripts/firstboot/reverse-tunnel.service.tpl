[Unit]
Description=FRP Client
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/frp/frpc -c /etc/pcrt/frpc.toml
Restart=always
RestartSec=5
StartLimitIntervalSec=0
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
