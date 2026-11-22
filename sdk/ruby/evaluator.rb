require "json"

module DekEvaluator
  VERSION = 1

  class Target
    def initialize(input, wire, parent_id)
      @input = input
      @wire = wire
      @parent_id = parent_id
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

    private

    def query(kind, key, location)
      @next_id += 1
      id = @next_id.to_s
      @wire.puts JSON.generate(protocolVersion: VERSION, parentId: @parent_id, id: id,
        kind: kind, key: key, origin: { file: location.path, line: location.lineno })
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
    while (line = input.gets)
      request = JSON.parse(line)
      id = request.fetch("id")
      entry = nil
      begin
        raise "incompatible protocolVersion" unless request["protocolVersion"] == VERSION

        operation = request.fetch("operation")
        raise "unsupported operation #{operation.inspect}" unless %w[collect expand].include?(operation)

        payload = request.fetch("payload")
        entry = payload.fetch("entry")
        target = Target.new(input, wire, id)
        result = Context.new(operation, payload.fetch("context"), target).run(entry)
        wire.puts JSON.generate(protocolVersion: VERSION, id: id, ok: true, result: result)
      rescue StandardError, ScriptError => error
        failure = { message: error.message, origin: error_location(error, entry) }
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
