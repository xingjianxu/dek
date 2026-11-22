file_resource(option("lab.platform.path"), source: source_file("source/demo.conf"), attributes: { mode: "0640" })
if option("lab.platform.alias")
  file_resource(option("lab.platform.alias"), source: source_file("source/demo.conf"))
end
