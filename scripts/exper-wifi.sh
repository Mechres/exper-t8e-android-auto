#!/bin/sh
# Tablet = Wi-Fi istemcisi: telefonun hotspot'una baglan (wpa_supplicant), adres al (udhcpc), kopunca yeniden dene.
# Servis: /etc/init.d/exper-wifi. Telefon adresi exper-auto tarafindan wlan0 varsayilan agi geciti olarak bulunur.
IF=wlan0
CONF=/etc/exper-auto/wpa_supplicant.conf

killall hostapd dnsmasq 2>/dev/null
ip addr flush dev $IF 2>/dev/null
ip link set $IF up
pidof wpa_supplicant >/dev/null || wpa_supplicant -B -i $IF -c $CONF -P /run/wpa_supplicant.pid

while true; do
	state=$(wpa_cli -i $IF status 2>/dev/null | sed -n 's/^wpa_state=//p')
	has_ip=$(ip -4 addr show dev $IF | grep -c 'inet ')
	if [ "$state" = "COMPLETED" ]; then
		[ "$has_ip" = "0" ] && udhcpc -i $IF -n -q -t 5 -T 2 >/dev/null 2>&1
	else
		# baglanti yok/koptu: eski adres ve rota kalmasin (exper-auto yanlis ag gecidine baglanmaya calismasin)
		[ "$has_ip" != "0" ] && ip addr flush dev $IF
	fi
	sleep 2
done
