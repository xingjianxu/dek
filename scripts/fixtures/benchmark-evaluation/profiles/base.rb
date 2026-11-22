service :openssh, enable: true
configure "environment.systemPackages", ["bash", "systemd"]
