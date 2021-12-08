#!/bin/bash
iptables -F

#sync nodes
while read ip; do
    iptables -A OUTPUT -p tcp -d "$ip" --dport 4132 -j ACCEPT
done <ips.txt

#top nodes
while read ip; do
    iptables -A OUTPUT -p tcp -d "$ip" --dport 4132 -j ACCEPT
done <ip.txt

iptables -A OUTPUT -p tcp --dport 4132 -j DROP
iptables -L