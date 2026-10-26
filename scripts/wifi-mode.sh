#!/bin/sh
# WiFi Mode Switcher: AP (In-Car Hotspot) <-> Client (Home Wi-Fi)

case "$1" in
    ap)
        echo "=== Switching wlan0 to Access Point mode (Exper-Auto) ==="
        # Stop client services
        rc-service net.wlan0 stop 2>/dev/null || true
        rc-service wpa_supplicant stop 2>/dev/null || true
        killall udhcpc 2>/dev/null || true

        # Reconfigure interface
        ip link set wlan0 down
        ip addr flush dev wlan0
        ip addr add 192.168.43.1/24 dev wlan0
        ip link set wlan0 up

        # Start DHCP server
        killall dnsmasq 2>/dev/null || true
        dnsmasq -C /etc/exper-auto/dnsmasq.conf

        # Start HostAPD
        killall hostapd 2>/dev/null || true
        hostapd -B /etc/exper-auto/hostapd.conf

        echo "Hotspot ACTIVE: SSID 'Exper-Auto', IP 192.168.43.1"
        ;;
    client)
        echo "=== Switching wlan0 to Client mode ==="
        killall hostapd 2>/dev/null || true
        killall dnsmasq 2>/dev/null || true

        ip link set wlan0 down
        ip addr flush dev wlan0
        ip link set wlan0 up

        rc-service wpa_supplicant start
        rc-service net.wlan0 start
        echo "Client mode ACTIVE (Connecting to saved Wi-Fi networks)"
        ;;
    status)
        echo "=== Wi-Fi Status ==="
        ip addr show dev wlan0
        if pidof hostapd > /dev/null; then
            echo "Mode: ACCESS POINT (Exper-Auto active)"
            echo "Connected Clients:"
            cat /var/lib/misc/dnsmasq.leases 2>/dev/null || true
        elif pidof wpa_supplicant > /dev/null; then
            echo "Mode: CLIENT"
            wpa_cli status 2>/dev/null | grep -E "wpa_state|ssid|ip_address" || true
        else
            echo "Mode: IDLE"
        fi
        ;;
    *)
        echo "Usage: $0 {ap|client|status}"
        exit 1
        ;;
esac
