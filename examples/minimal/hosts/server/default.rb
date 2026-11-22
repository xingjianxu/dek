profile "base"
service :caddy, enable: true, configFile: source_file("./Caddyfile")
