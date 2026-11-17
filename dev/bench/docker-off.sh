#!/usr/bin/env bash
# Stop Docker and undo what it did to the host's network: its firewall (iptables FORWARD
# policy DROP) and its bridge block openpod's root bridge networks. Dev VM only.
set -uo pipefail
sudo systemctl stop docker.service docker.socket containerd.service >/dev/null 2>&1
sudo iptables -P FORWARD ACCEPT 2>/dev/null
for chain in DOCKER-USER DOCKER-FORWARD DOCKER DOCKER-ISOLATION-STAGE-1 DOCKER-ISOLATION-STAGE-2 DOCKER-CT DOCKER-BRIDGE; do
  sudo iptables -F "$chain" 2>/dev/null
done
sudo iptables -F FORWARD 2>/dev/null
sudo iptables -t nat -F 2>/dev/null
sudo ip link del docker0 2>/dev/null
true
