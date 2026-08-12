#!/bin/sh

if ping -c 1 1.1.1.1 >/dev/null 2>&1; then
  echo "Ethernet is OK"
else
  echo "No Ethernet"
fi