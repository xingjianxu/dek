require "digest"

module DekEvaluator
  module Expansion
    def options
      context.fetch("options")
    end

    def option(path)
      options[path.to_s]
    end

    def resource(kind, depends_on: [], notifications: [], **attributes)
      record_resource({ ready: { kind.to_s => attributes } }, caller_locations(1, 1).first,
        depends_on, notifications)
    end

    def file_resource(path, content: nil, source: nil, attributes: {}, depends_on: [], notifications: [])
      raise ArgumentError, "file_resource requires exactly one of content and source" if content.nil? == source.nil?

      location = caller_locations(1, 1).first
      value = if source
        { relative: { path: String(source), origin: resource_origin(location) } }
      else
        { inline: String(content).bytes }
      end
      record_resource({ file: { path: path, attributes: attributes, content: value } }, location,
        depends_on, notifications)
    end

    def content_object(content)
      bytes = String(content)
      key = "sha256:#{Digest::SHA256.hexdigest(bytes)}"
      module_output[:contents][key] = bytes.bytes
      key
    end

    private

    def evaluate_source(entry)
      result = super
      @module_output || result
    end

    def module_output
      raise "module resource helpers require expand" unless operation == "expand"

      @module_output ||= { resources: [], contents: {} }
    end

    def record_resource(desired, location, depends_on, notifications)
      module_output[:resources] << {
        desired: desired, origins: [resource_origin(location)], dependsOn: depends_on,
        notifications: notifications
      }
      nil
    end

    def resource_origin(location)
      { file: location.path, line: location.lineno }
    end
  end

  Context.prepend(Expansion)
end
