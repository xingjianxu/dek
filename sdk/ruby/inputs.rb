require "digest"
require "json"

module DekEvaluator
  # Load the digest implementation before observing user code.
  Digest::SHA256.digest("")
  NATIVE_REQUIRE = Kernel.instance_method(:require)

  class Inputs
    attr_reader :observations

    def self.current
      Thread.current.thread_variable_get(:dek_evaluation_inputs)
    end

    def initialize(root, code)
      @root = File.expand_path(root)
      @code = code
      @observations = []
      @code_files = {}
    end

    def active
      previous = Inputs.current
      Thread.current.thread_variable_set(:dek_evaluation_inputs, self)
      yield
    ensure
      Thread.current.thread_variable_set(:dek_evaluation_inputs, previous)
    end

    def location_origin(location)
      origin(location.absolute_path || location.path, location.lineno)
    end

    def origin(path, line)
      { file: file_id(path), line: line }
    end

    def entry_text(path)
      bytes = read_file(path)
      observe_code(path, digest(bytes), nil)
      utf8(bytes)
    end

    def read_text(path, location)
      file = resolve_path(path, location)
      bytes = read_file(file)
      record("file", file_id(file), digest(bytes), location_origin(location))
      utf8(bytes)
    rescue Errno::ENOENT
      record("file", file_id(file), "missing", location_origin(location))
      nil
    end

    def read_dir(path, location)
      directory = resolve_path(path, location)
      names = Dir.children(directory).map { |name| utf8(name) }.sort
      record("directory", file_id(directory), digest(JSON.generate(names)), location_origin(location))
      names
    rescue Errno::ENOENT
      record("directory", file_id(directory), "missing", location_origin(location))
      nil
    end

    def env(name, location)
      raise TypeError, "environment name must be a string" unless name.is_a?(String)

      value = ENV[name]
      record("environment", name, value ? digest(value) : "missing", location_origin(location))
      value && utf8(value)
    end

    def track_require(feature, location, &require_file)
      @code.track_require(feature, self, location_origin(location), &require_file)
    end

    def track_load(feature, location, &load_file)
      @code.track_load(feature, self, location_origin(location), &load_file)
    end

    def observe_code(file, observation, origin)
      id = file_id(file)
      if (prior = @code_files[id])
        raise IOError, "loaded Ruby source changed during evaluation: #{id}" if prior != observation

        return
      end

      @code_files[id] = observation
      record("file", id, observation, origin)
    end

    def self.file_digest(path)
      raise IOError, "not a regular file: #{path}" unless File.stat(path).file?

      { digest: "sha256:#{Digest::SHA256.hexdigest(File.binread(path))}" }
    end

    def self.file_observation(path)
      file_digest(path)
    rescue Errno::ENOENT
      "missing"
    end

    private

    def file_id(path)
      absolute = File.expand_path(path)
      prefix = @root.end_with?("/") ? @root : "#{@root}/"
      absolute.delete_prefix(prefix)
    end

    def resolve_path(path, location)
      File.expand_path(File.path(path), File.dirname(location.absolute_path || location.path))
    end

    def read_file(path)
      raise IOError, "not a regular file: #{path}" unless File.stat(path).file?

      File.binread(path)
    end

    def digest(bytes)
      { digest: "sha256:#{Digest::SHA256.hexdigest(bytes)}" }
    end

    def utf8(bytes)
      value = bytes.dup.force_encoding(Encoding::UTF_8)
      raise EncodingError, "SDK text is not valid UTF-8" unless value.valid_encoding?

      value
    end

    def record(kind, id, observation, origin)
      @observations << { kind: kind, id: id, observation: observation, origin: origin }
    end
  end

  class CodeDependencies
    def initialize
      @requires = {}
      @loaded = {}
      @stack = []
    end

    def track_require(feature, inputs, origin)
      key = require_key(feature)
      before = $LOADED_FEATURES.dup
      previously_loaded = loaded_feature(feature, before)
      unless previously_loaded
        source, missing = source_path(feature)
        if source
          real_source = File.realpath(source)
          previous_digest = Inputs.file_digest(source)
        end
      end
      files = {}
      @stack.push(files)
      begin
        loaded = yield
        if loaded
          added = $LOADED_FEATURES - before
          added.each do |file|
            files[file] ||= Inputs.file_digest(file) if File.absolute_path?(file)
          end
          selected = loaded_feature(feature, added) || added.last
          if source && (selected == source || selected == real_source)
            if files[selected] != previous_digest || File.realpath(source) != real_source
              raise IOError, "loaded Ruby source changed during evaluation: #{source}"
            end
            files[source] ||= previous_digest
            missing.each { |file| files[file] ||= "missing" }
          end
          @requires[key] = files
          @loaded[selected] = files if selected
        else
          file = previously_loaded || loaded_feature(feature, $LOADED_FEATURES)
          if (known = @requires[key] || @loaded[file])
            known.each do |dependency, observation|
              if Inputs.file_observation(dependency) != observation
                raise IOError, "loaded Ruby source changed during evaluation: #{dependency}"
              end
              files[dependency] ||= observation
            end
          elsif file && File.file?(file)
            files[file] ||= Inputs.file_digest(file)
          end
        end
      ensure
        @stack.pop
      end
      files.each { |file, observation| inputs.observe_code(file, observation, origin) }
      @stack.last&.merge!(files)
      loaded
    end

    def track_load(feature, inputs, origin)
      candidates = feature_paths(feature) << File.expand_path(feature)
      if (file = candidates.find { |candidate| File.file?(candidate) })
        observation = Inputs.file_digest(file)
      end
      result = yield
      if file
        if Inputs.file_digest(file) != observation
          raise IOError, "loaded Ruby source changed during evaluation: #{file}"
        end
        inputs.observe_code(file, observation, origin)
        @stack.last&.store(file, observation)
      end
      result
    end

    private

    def require_key(feature)
      spelling = feature.delete_suffix(".rb")
      if File.absolute_path?(spelling) || spelling.start_with?("./", "../")
        File.expand_path(spelling)
      else
        [spelling, Dir.pwd, $LOAD_PATH.dup]
      end
    end

    def loaded_feature(feature, features)
      candidates = feature_paths(feature)
      exact = features.find { |file| candidates.include?(file) }
      return exact if exact

      unless feature.end_with?(".rb", ".so", ".bundle")
        return features.find do |file|
          candidates.include?(file.delete_suffix(".rb").delete_suffix(".so").delete_suffix(".bundle"))
        end
      end

      nil
    end

    def source_path(feature)
      return [nil, []] if feature.end_with?(".so", ".bundle")

      candidates = feature_paths(feature)
      candidates.map! { |path| "#{path}.rb" } unless feature.end_with?(".rb")
      missing = []
      candidates.each do |candidate|
        return [candidate, missing] if File.stat(candidate).file?
      rescue Errno::ENOENT
        missing << candidate
      rescue SystemCallError
        # Leave inaccessible candidates to Ruby's ordinary lookup.
      end
      [nil, missing]
    end

    def feature_paths(feature)
      if File.absolute_path?(feature) || feature.start_with?("./", "../")
        [File.expand_path(feature)]
      else
        $LOAD_PATH.map { |directory| File.expand_path(feature, directory) }
      end
    end
  end

  module ObserveCode
    private

    def require(feature)
      inputs = Inputs.current
      return super unless inputs

      inputs.track_require(File.path(feature), caller_locations(1, 1).first) { super }
    end

    def require_relative(feature)
      location = caller_locations(1, 1).first
      base = location.absolute_path || location.path
      raise LoadError, "cannot infer basepath" if base == "-e" || base.start_with?("(")

      absolute = File.expand_path(File.path(feature), File.dirname(base))
      inputs = Inputs.current
      return NATIVE_REQUIRE.bind_call(self, absolute) unless inputs

      inputs.track_require(absolute, location) { NATIVE_REQUIRE.bind_call(self, absolute) }
    end

    def load(feature, wrap = false)
      inputs = Inputs.current
      return super unless inputs

      inputs.track_load(File.path(feature), caller_locations(1, 1).first) { super }
    end
  end

  module ObservePublicCode
    include ObserveCode
    public :require, :require_relative, :load
  end

  Kernel.prepend(ObserveCode)
  Kernel.singleton_class.prepend(ObservePublicCode)
end
