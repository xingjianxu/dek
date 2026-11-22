module DekEvaluator
  module Declarations
    OMITTED_VALUE = Object.new.freeze

    def profile(name, params = nil, **named)
      raise ArgumentError, "pass profile parameters as a map or keywords" if params && !named.empty?
      params ||= named
      raise TypeError, "profile parameters must be a map" unless params.is_a?(Hash)
      name = name.to_s if name.is_a?(Symbol)
      unless name.is_a?(String) && !name.strip.empty? && !name.match?(/[[:cntrl:]]/)
        raise ArgumentError, "profile name must be a nonempty string"
      end

      @imports << {
        name: name.dup,
        params: json_value(params),
        origin: @inputs.location_origin(caller_locations(1, 1).first)
      }
      nil
    end

    def configure(path, value = OMITTED_VALUE, **fields)
      location = caller_locations(1, 1).first
      if value.equal?(OMITTED_VALUE)
        raise ArgumentError, "configure requires a value or option fields" if fields.empty?

        declare_fields(path, fields, location)
      else
        raise ArgumentError, "configure cannot combine a value and option fields" unless fields.empty?

        declare(path, value, location)
      end
      nil
    end

    def service(name, **fields)
      declare_fields("services.#{component(name)}", fields, caller_locations(1, 1).first)
      nil
    end

    def user(name, **fields)
      declare_account("users.users.#{component(name)}", fields, caller_locations(1, 1).first)
      nil
    end

    def group(name, **fields)
      declare_account("users.groups.#{component(name)}", fields, caller_locations(1, 1).first)
      nil
    end

    def directory(path, owner: nil, group: nil, mode: nil, config: "dek")
      unless path.is_a?(String) && !path.empty?
        raise ArgumentError, "directory requires a target path"
      end

      attributes = { user: owner, group: group, mode: mode }.compact
      declare("systemd.tmpfiles.settings.#{component(config)}",
        { path => { d: attributes } }, caller_locations(1, 1).first)
      nil
    end

    private

    def declare_account(path, fields, location)
      if fields.empty?
        declare(path, {}, location)
      else
        declare_fields(path, fields, location)
      end
    end

    def declare_fields(path, fields, location)
      validate_path(path)
      fields.each { |name, value| declare("#{path}.#{component(name)}", value, location) }
    end

    def declare(path, value, location)
      validate_path(path)
      @options << { path: path.dup, value: json_value(value), origin: @inputs.location_origin(location) }
    end

    def validate_path(path)
      unless path.is_a?(String) && !path.empty? && !path.split(".", -1).include?("") && !path.match?(/[[:cntrl:]]/)
        raise ArgumentError, "invalid option path #{path.inspect}"
      end
    end

    def component(value)
      value = value.to_s if value.is_a?(Symbol)
      validate_path(value)
      raise ArgumentError, "invalid option path component #{value.inspect}" if value.include?(".")

      value
    end

    def json_value(value)
      case value
      when NilClass, TrueClass, FalseClass, Integer
        value
      when Float
        raise TypeError, "option numbers must be finite" unless value.finite?

        value
      when String, Symbol
        value.to_s.dup
      when Array
        value.map { |item| json_value(item) }
      when Hash
        value.each_with_object({}) do |(key, item), object|
          unless key.is_a?(String) || key.is_a?(Symbol)
            raise TypeError, "option object keys must be strings or symbols"
          end
          name = key.to_s
          raise TypeError, "duplicate option object key #{name.inspect}" if object.key?(name)

          object[name] = json_value(item)
        end
      else
        raise TypeError, "unsupported option value #{value.class}"
      end
    end
  end
end
