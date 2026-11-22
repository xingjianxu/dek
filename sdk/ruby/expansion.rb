require "digest"

module DekEvaluator
  module Expansion
    OptionSource = Struct.new(:path, :origin)

    def options
      context.fetch("options")
    end

    def option(path)
      options[path.to_s]
    end

    def option_source(path)
      path = path.to_s
      value = option(path)
      raise ArgumentError, "option #{path.inspect} does not specify a source file" unless value.is_a?(String)

      origin = context.fetch("optionOrigins").fetch(path)
      OptionSource.new(value, origin)
    end

    def resource(kind, depends_on: [], notifications: [], **attributes)
      record_resource({ ready: { kind.to_s => attributes } }, caller_locations(1, 1).first,
        depends_on, notifications)
    end

    def file_resource(path, content: nil, source: nil, attributes: {}, depends_on: [], notifications: [])
      raise ArgumentError, "file_resource requires exactly one of content and source" if content.nil? == source.nil?

      location = caller_locations(1, 1).first
      value = if source.is_a?(OptionSource)
        { relative: { path: source.path, origin: source.origin } }
      elsif source.is_a?(String)
        { relative: { path: source, origin: resource_origin(location) } }
      elsif content.is_a?(String)
        { inline: content.bytes }
      else
        raise ArgumentError, "file resource content and source paths require strings"
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
      @inputs.location_origin(location)
    end
  end

  SdkContext.prepend(Expansion)
end
