require "json"
require_relative "inputs" unless defined?(DekEvaluator::Inputs)
require_relative "declarations" unless defined?(DekEvaluator::Declarations)

module DekEvaluator
  VERSION = 1

  class Target
    def initialize(input, wire, parent_id, inputs)
      @input = input
      @wire = wire
      @parent_id = parent_id
      @inputs = inputs
      @next_id = 0
    end

    def fact(name)
      query("fact", name, caller_locations(1, 1).first)
    end

    def capability(name)
      query("capability", name, caller_locations(1, 1).first)
    end

    def read_text(path)
      query("readText", path, caller_locations(1, 1).first)
    end

    def http_input(url, location)
      query("http", url, location)
    end

    private

    def query(kind, key, location)
      @next_id += 1
      id = @next_id.to_s
      origin = if @inputs
        @inputs.location_origin(location)
      else
        { file: location.path, line: location.lineno }
      end
      @wire.puts JSON.generate(protocolVersion: VERSION, parentId: @parent_id, id: id,
        kind: kind, key: key, origin: origin)
      response = JSON.parse(@input.readline)
      unless response["protocolVersion"] == VERSION &&
          response["parentId"] == @parent_id && response["id"] == id
        raise "invalid target reply"
      end
      response.fetch("result")
    end
  end

  class Context
    attr_reader :operation, :context, :target

    def initialize(operation, context, target)
      @operation = operation
      @context = context
      @target = target
    end

    def run(entry)
      directory = Dir.pwd
      environment = ENV.to_h
      evaluate_source(entry)
    ensure
      ENV.replace(environment) if environment
      Dir.chdir(directory) if directory
    end

    private

    def evaluate_source(entry)
      instance_eval(File.read(entry, encoding: "UTF-8"), entry, 1)
    end
  end

  class SdkContext < Context
    def initialize(operation, context, target, inputs)
      super(operation, context, target)
      @inputs = inputs
    end

    def params
      context.fetch("params", {})
    end

    def source_file(path)
      path = File.path(path)
      rooted_on_windows = File::ALT_SEPARATOR && (path.start_with?(File::ALT_SEPARATOR) || path.match?(/\A[A-Za-z]:/))
      if path.empty? || path.include?("\0") || File.absolute_path?(path) || rooted_on_windows
        raise ArgumentError, "source_file requires a relative path"
      end

      path
    end

    def read_text(path)
      @inputs.read_text(path, caller_locations(1, 1).first)
    end

    def read_dir(path)
      @inputs.read_dir(path, caller_locations(1, 1).first)
    end

    def env(name)
      @inputs.env(name, caller_locations(1, 1).first)
    end

    def http_text(url)
      unless url.is_a?(String) && url.match?(/\Ahttps?:\/\/[^\/?#]+/i) && !url.match?(/[[:space:][:cntrl:]]/)
        raise ArgumentError, "http_text requires an HTTP or HTTPS URL"
      end

      target.http_input(url, caller_locations(1, 1).first)
    end

    private

    def evaluate_source(entry)
      @inputs.active do
        instance_eval(@inputs.entry_text(entry), entry, 1)
      end
    end
  end

  class DeclarationContext < SdkContext
    include Declarations

    def run(entry)
      @imports = []
      @options = []
      super
      { imports: @imports, options: @options, inputs: [] }
    end
  end

  def self.error_location(error, entry)
    return nil unless entry

    if error.is_a?(SyntaxError)
      location = error.message.match(/\A(.+?):([1-9]\d*):/)
      return { file: location[1], line: location[2].to_i } if location
    end
    location = error.backtrace_locations&.find do |line|
      line.path == entry || (line.absolute_path && line.path.end_with?(".rb"))
    end
    location && { file: location.path, line: location.lineno }
  end

  def self.run(input, wire)
    code = CodeDependencies.new
    while (line = input.gets)
      request = JSON.parse(line)
      id = request.fetch("id")
      entry = nil
      inputs = nil
      begin
        raise "incompatible protocolVersion" unless request["protocolVersion"] == VERSION

        operation = request.fetch("operation")
        raise "unsupported operation #{operation.inspect}" unless %w[collect expand].include?(operation)

        payload = request.fetch("payload")
        entry = payload.fetch("entry")
        inputs = Inputs.new(payload["repoRoot"], code) if payload["repoRoot"]
        target = Target.new(input, wire, id, inputs)
        context = payload.fetch("context")
        evaluation = if inputs && operation == "collect"
          DeclarationContext.new(operation, context, target, inputs)
        elsif inputs
          SdkContext.new(operation, context, target, inputs)
        else
          Context.new(operation, context, target)
        end
        result = evaluation.run(entry)
        response = { protocolVersion: VERSION, id: id, ok: true, result: result }
        response[:inputs] = inputs.observations if inputs
        wire.puts JSON.generate(response)
      rescue StandardError, ScriptError => error
        origin = error_location(error, entry)
        origin = inputs.origin(origin[:file], origin[:line]) if inputs && origin
        failure = { message: error.message, origin: origin }
        wire.puts JSON.generate(protocolVersion: VERSION, id: id, ok: false, error: failure)
      end
    end
  end
end

input = STDIN.dup
STDIN.reopen(File::NULL)
$stdin = STDIN
wire = STDOUT.dup
wire.sync = true
STDOUT.reopen(STDERR)
$stdout = STDERR
DekEvaluator.run(input, wire)
