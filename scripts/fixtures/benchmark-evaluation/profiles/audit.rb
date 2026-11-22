configure "systemd.services", {
  "dek-journal-audit.service" => {
    "description" => "Record system journal disk usage",
    "serviceConfig" => {
      "Type" => "oneshot",
      "ExecStart" => "/usr/bin/journalctl --disk-usage"
    }
  }
}
configure "systemd.timers", {
  "dek-journal-audit.timer" => {
    "description" => "Periodically record system journal disk usage",
    "timerConfig" => {"OnCalendar" => "daily", "Persistent" => true},
    "wantedBy" => ["timers.target"], "enabled" => true, "running" => true
  }
}
