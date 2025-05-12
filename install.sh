set -e

if [ "$(id -u)" -ne 0 ]; then
    echo "Please run this script as root."
    exit 1
fi

if ! id -u msparking &>/dev/null; then
    useradd -r -s /bin/false msparking
fi

install -d -m 755 -o msparking -g msparking /opt/msparking
install -d -m 755 -o msparking -g msparking /opt/msparking/config

install -m 755 -o msparking -g msparking ./target/release/msparking /opt/msparking/

# If the config directory is not empty, do not overwrite it
if [ -d /opt/msparking/config ] && [ "$(ls -A /opt/msparking/config)" ]; then
    echo "Config directory is not empty, skipping config file installation."
else
install -m 644 -o msparking -g msparking ./config/default.toml /opt/msparking/config/
fi

install -m 644 ./msparking.service /etc/systemd/system/

systemctl daemon-reload

echo "Installed."
echo "Please edit config under /opt/msparking/config/default.toml"
echo "And start the service using:"
echo "  systemctl enable --now msparking.service"