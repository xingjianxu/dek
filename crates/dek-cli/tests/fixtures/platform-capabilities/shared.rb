configure "lab.platform", enable: true, path: params.fetch("path")
profile :timer if target.capability("systemdUnit.content")
