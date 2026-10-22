# frozen_string_literal: true

# This file contains code derived from Homebrew (https://github.com/Homebrew/brew)
# Copyright (c) 2009-present, Homebrew contributors
# Licensed under BSD 2-Clause License (see LICENSE-HOMEBREW)
#
# Portions of this code implement a compatibility shim that mimics Homebrew's
# Formula DSL and helper methods to allow Homebrew formulas to run in NextBrew.
#
# Homebrew Compatibility: 5.0.x
# This shim has been tested with Homebrew 5.0.14.
# Last verified: 2025-02-13

require "fileutils"

# Formulae are UTF-8, whatever the build's (cleared) locale says.
Encoding.default_external = Encoding::UTF_8
Encoding.default_internal = Encoding::UTF_8
require "pathname"
require "json"
require "tmpdir"
require "tempfile"
require "uri"
require "set"
require "find"
require "digest/sha2"

module NextBrewChecksum
  module_function

  def verify_file!(path, expected_sha256, context)
    return if expected_sha256.nil? || expected_sha256.to_s.strip.empty?

    expected = normalize_sha256!(expected_sha256, context)
    actual = Digest::SHA256.file(path).hexdigest
    return if actual == expected

    $stderr.puts "Error: checksum mismatch for #{context} (expected #{expected}, got #{actual})"
    exit 1
  end

  def normalize_sha256!(value, context)
    normalized = value.to_s.strip.downcase
    unless normalized.match?(/\A[0-9a-f]{64}\z/)
      $stderr.puts "Error: invalid sha256 checksum for #{context}: expected 64 hex chars, got #{normalized.length}"
      exit 1
    end
    normalized
  end
end

NEXTBREW_PREFIX = ENV.fetch("NEXTBREW_PREFIX")
NEXTBREW_CELLAR = ENV.fetch("NEXTBREW_CELLAR")
FORMULA_NAME = ENV.fetch("NEXTBREW_FORMULA_NAME")
FORMULA_VERSION = ENV.fetch("NEXTBREW_FORMULA_VERSION")
FORMULA_FILE = ENV.fetch("NEXTBREW_FORMULA_FILE")
INSTALLED_DEPS = JSON.parse(ENV.fetch("NEXTBREW_INSTALLED_DEPS", "{}"))
# Where the source was unpacked, wherever the formula goes from there.
BUILDPATH = Pathname.new(Dir.pwd)
# Homebrew's own, which formulae use directly.
HOMEBREW_PREFIX = Pathname.new(NEXTBREW_PREFIX)
HOMEBREW_CELLAR = Pathname.new(NEXTBREW_CELLAR)
HOMEBREW_LIBRARY = HOMEBREW_PREFIX + "Library"

# Where each `system` call's output goes (`Formula#logs`), counted as
# Homebrew numbers them, and how a failed one stops the build.
module NextBrewLogs
  class << self
    attr_writer :count

    def count
      @count ||= 0
    end

    def dir
      ENV.fetch("NEXTBREW_LOGS")
    end

    def verbose?
      !ENV.fetch("HOMEBREW_VERBOSE", "").empty?
    end

    # The executor tells a failed command, already shown, from any other
    # failure of the build.
    def failed!
      File.write(ENV.fetch("NEXTBREW_BUILD_FAILED"), "")
      exit 1
    end
  end
end

module OS
  def self.mac?
    RUBY_PLATFORM.include?("darwin")
  end

  def self.linux?
    RUBY_PLATFORM.include?("linux")
  end

  # `OS.kernel_version`: `uname -r`, as a version.
  def self.kernel_version
    FormulaVersion.new(`uname -r`.strip)
  end

  module Mac
    # `MacOS.version`: the major version from 11 on, as `strip_patch`
    # makes it, else `10.x`.
    def self.version
      return MacOSVersion.new("0") unless OS.mac?

      @version ||= begin
        full = `/usr/bin/sw_vers -productVersion`.strip
        major, minor = full.split(".")
        MacOSVersion.new(major.to_i >= 11 ? major : "#{major}.#{minor}")
      end
    end

    def self.full_version
      MacOSVersion.new(`/usr/bin/sw_vers -productVersion`.strip)
    end
  end
end

# Homebrew's top-level `MacOS`.
module MacOS
  def self.version; OS::Mac.version; end
  def self.full_version; OS::Mac.full_version; end

  def self.sdk_path
    Pathname.new(`/usr/bin/xcrun --show-sdk-path`.strip)
  end

  def self.active_developer_dir
    `/usr/bin/xcode-select -p`.strip
  end
end

# Homebrew's `Utils` command helpers.
module Utils
  def self.popen_read(*args, **options)
    IO.popen(args.map(&:to_s), err: options.fetch(:err, File::NULL), &:read).to_s
  end

  # As `popen_read`, but failing the build when the command does.
  def self.safe_popen_read(*args, **options)
    output = popen_read(*args, **options)
    odie "#{args.join(" ")} failed" unless $?.success?
    output
  end
end

class MacOSVersion
  include Comparable

  SYMBOLS = {
    tahoe: "26", sequoia: "15", sonoma: "14", ventura: "13", monterey: "12",
    big_sur: "11", catalina: "10.15", mojave: "10.14"
  }.freeze

  def initialize(version)
    version = SYMBOLS.fetch(version, version.to_s) if version.is_a?(Symbol)
    @version = version.to_s
    @major = @version.split(".").first.to_i
  end

  # As versions, so `10.15 < 11` and `version >= :sonoma` hold.
  def <=>(other)
    other = MacOSVersion.new(other) unless other.is_a?(MacOSVersion)
    Gem::Version.new(@version) <=> Gem::Version.new(other.to_s)
  end

  def to_s; @version; end
  def to_i; @major; end
end

module Hardware
  module CPU
    def self.arm?
      RUBY_PLATFORM.include?("arm") || RUBY_PLATFORM.include?("aarch64")
    end

    def self.intel?
      RUBY_PLATFORM.include?("x86_64")
    end

    def self.is_64_bit?
      true
    end

    # `Hardware::CPU.arch`: `:arm64` or `:x86_64`.
    def self.arch
      arm? ? :arm64 : :x86_64
    end
  end
end

module DevelopmentTools
  def self.ld64_version
    return MacOSVersion.new("0") unless OS.mac?
    raw = `xcrun ld -version_details 2>/dev/null`.strip rescue ""
    m = raw.match(/"version"\s*:\s*"([^"]+)"/)
    MacOSVersion.new(m ? m[1] : "0")
  end

  # `DevelopmentTools.clang_build_version`: Apple clang's build number
  # (`clang-1700.0.13` is 1700), compared with numbers.
  def self.clang_build_version
    return 0 unless OS.mac?

    @clang_build_version ||= begin
      out = IO.popen(["/usr/bin/clang", "--version"], err: File::NULL, &:read).to_s
      out[/clang-(\d+)/, 1].to_i
    end
  end
end

# ActiveSupport's helpers that formulae use.
class Object
  def blank?
    respond_to?(:empty?) ? !!empty? : !self
  end

  def present?
    !blank?
  end
end

class String
  def blank?
    strip.empty?
  end

  def exclude?(string)
    !include?(string)
  end
end

module Enumerable
  def exclude?(object)
    !include?(object)
  end
end

module Kernel
  def odie(message)
    $stderr.puts "Error: #{message}"
    exit 1
  end
end

class FormulaVersion < String
  def major
    FormulaVersion.new(split(".")[0] || self)
  end

  def minor
    FormulaVersion.new(split(".")[1] || "0")
  end

  def patch
    FormulaVersion.new(split(".")[2] || "0")
  end

  def major_minor
    FormulaVersion.new("#{major}.#{minor}")
  end

  def to_i
    major.to_s.to_i
  end
end

module Homebrew
  module EnvExtension
    def append(key, value, separator = " ")
      existing = self[key]
      self[key] = existing && !existing.empty? ? "#{existing}#{separator}#{value}" : value.to_s
    end

    def prepend(key, value, separator = " ")
      existing = self[key]
      self[key] = existing && !existing.empty? ? "#{value}#{separator}#{existing}" : value.to_s
    end

    def append_path(key, path)
      append(key, path, ":")
    end

    def prepend_path(key, path)
      prepend(key, path, ":")
    end

    def prepend_create_path(key, path)
      FileUtils.mkdir_p(path.to_s)
      prepend(key, path, ":")
    end

    # The compilers, as superenv names them.
    def cc; self["CC"]; end
    def cxx; self["CXX"]; end
    def cflags; self["CFLAGS"]; end
    def cxxflags; self["CXXFLAGS"]; end
    def cppflags; self["CPPFLAGS"]; end
    def ldflags; self["LDFLAGS"]; end
    def compiler; :clang; end

    # `ENV.make_jobs`: how many jobs a build runs.
    def make_jobs
      fetch("HOMEBREW_MAKE_JOBS", "1").to_i
    end

    # Superenv's flag helpers, here on the flags themselves.
    CC_FLAG_VARS = %w[CFLAGS CXXFLAGS OBJCFLAGS OBJCXXFLAGS].freeze

    def append_to_cflags(value)
      CC_FLAG_VARS.each { |key| append(key, value) }
    end

    def remove_from_cflags(value)
      CC_FLAG_VARS.each do |key|
        self[key] = self[key].to_s.split.reject { |flag| value === flag }.join(" ") if key?(key)
      end
    end

    def O0; append_to_cflags("-O0"); end
    def O1; append_to_cflags("-O1"); end
    def O3; append_to_cflags("-O3"); end
    def cxx11; append("CXXFLAGS", "-std=c++11"); end
    def libcxx; append("CXXFLAGS", "-stdlib=libc++"); end
    def runtime_cpu_detection; nil; end
    def permit_arch_flags; nil; end
    def permit_weak_imports; nil; end
    def refurbish_args; nil; end

    # Homebrew's: build with one job, for the block or from now on.
    def deparallelize
      old = delete("MAKEFLAGS")
      return unless block_given?

      begin
        yield
      ensure
        self["MAKEFLAGS"] = old
      end
    end
  end
end
ENV.extend(Homebrew::EnvExtension)

class StableSpec
  attr_reader :url, :mirrors
  attr_accessor :version

  def initialize(url, mirrors)
    @url = url
    @mirrors = mirrors
  end
end

# Homebrew's `Language::Python`: its virtualenvs, made with the formula's
# Python and filled with pip, as Homebrew's are.
module Language
  module Python
    def self.major_minor_version(python)
      out = IO.popen([python.to_s, "--version"], err: [:child, :out], &:read).to_s
      version = out[/(\d\.\d+)/, 1]
      version && FormulaVersion.new(version)
    end

    def self.site_packages(python)
      "lib/python#{major_minor_version(python)}/site-packages"
    end

    module Virtualenv
      def virtualenv_create(venv_root, python = "python", formula = self,
                            system_site_packages: true, without_pip: true)
        venv = Venv.new(formula, venv_root, python)
        venv.create(system_site_packages: system_site_packages, without_pip: without_pip)

        # The dependencies' own site-packages, on the virtualenv's path.
        pth = INSTALLED_DEPS.keys.reject { |dep| python_names.include?(dep) }.filter_map do |dep|
          dir = HOMEBREW_PREFIX + "opt" + dep + Language::Python.site_packages(python)
          "import site; site.addsitedir('#{dir}')\n" if dir.exist?
        end
        (venv.site_packages + "homebrew_deps.pth").write(pth.join) unless pth.empty?
        venv
      end

      def needs_python?(python)
        self.class.dependency_names.include?(python)
      end

      def virtualenv_install_with_resources(using: nil, system_site_packages: true, without_pip: true,
                                            link_manpages: true, without: nil, start_with: nil, end_with: nil)
        python = using
        if python.nil?
          wanted = python_names.select { |py| needs_python?(py) }
          odie "#{name}: no Python dependency to build with" if wanted.empty?
          odie "#{name}: more than one Python dependency" if wanted.size > 1

          python = wanted.first
          python = "python3" if python == "python"
        end

        remaining = resources.to_h { |r| [r.name, r] }
        take = ->(names) { Array(names).map { |n| remaining.delete(n) or odie("Resource \"#{n}\" is not defined") } }
        take.call(without)
        first = take.call(start_with)
        last = take.call(end_with)
        venv_resources = first + remaining.values + last

        venv = virtualenv_create(libexec, python.delete("@"), system_site_packages: system_site_packages,
                                                              without_pip: without_pip)
        venv.pip_install(venv_resources.reject(&:test?))
        venv.pip_install_and_link(buildpath, link_manpages: link_manpages)
        venv
      end

      def python_names
        %w[python python3 pypy pypy3] + self.class.dependency_names.select { |n| n.start_with?("python@") }
      end

      class Venv
        def initialize(formula, venv_root, python)
          @formula = formula
          @venv_root = Pathname(venv_root)
          @python = python
        end

        def root; @venv_root; end

        def site_packages
          @venv_root + Language::Python.site_packages(@python)
        end

        def create(system_site_packages: true, without_pip: true)
          return if (@venv_root + "bin/python").exist?

          args = ["-m", "venv"]
          args << "--system-site-packages" if system_site_packages
          args << "--without-pip" if without_pip
          @formula.system @python, *args, @venv_root

          # Point at the Python's `opt`, not its versioned keg.
          cellar = HOMEBREW_CELLAR.to_s
          @venv_root.find do |f|
            next unless f.symlink?
            next unless f.readlink.expand_path.to_s.start_with?(cellar)

            rp = f.realpath.to_s
            version = rp[%r{^#{Regexp.escape(cellar)}/python@(.*?)/}, 1]
            version = "@#{version}" if version
            target = rp.sub(%r{#{Regexp.escape(cellar)}/python#{version}/[^/]+},
                            @formula.formula_opt_prefix("python#{version}").to_s)
            f.unlink
            f.make_symlink(target)
          end
          lib64 = @venv_root + "lib64"
          lib64.make_symlink("lib") unless lib64.exist?
          cfg_file = @venv_root + "pyvenv.cfg"
          if cfg_file.exist?
            cfg = cfg_file.read
            framework = "Frameworks/Python.framework/Versions"
            match = cfg.match(%r{= *(#{Regexp.escape(cellar)}/(python@[\d.]+)/[^/]+(?:/#{framework}/[\d.]+)?/bin)})
            if match
              File.write(cfg_file, cfg.sub(match[1], @formula.formula_opt_bin(match[2]).to_s))
            end
          end
          (@venv_root + "bin").glob("[Aa]ctivate*").each(&:unlink)
        end

        def pip_install(targets, build_isolation: true)
          Array(targets).each do |t|
            if t.is_a?(StagedResource)
              t.stage do
                target = Pathname.pwd
                target += File.basename(URI.parse(t.url).path) if t.url&.match?(/[.-]py3[^-]*-none-any\.whl$/)
                do_install(target, build_isolation: build_isolation)
              end
            else
              t = t.lines.map(&:strip) if t.is_a?(String) && t.include?("\n")
              do_install(t, build_isolation: build_isolation)
            end
          end
        end

        def pip_install_and_link(targets, link_manpages: true, build_isolation: true)
          bin_before = Dir[@venv_root + "bin/*"].to_set
          man_before = Dir[@venv_root + "share/man/man*/*"].to_set
          pip_install(targets, build_isolation: build_isolation)
          bin_to_link = (Dir[@venv_root + "bin/*"].to_set - bin_before).to_a
          @formula.bin.install_symlink(bin_to_link)
          return unless link_manpages

          (Dir[@venv_root + "share/man/man*/*"].to_set - man_before).each do |manpage|
            (@formula.man + Pathname.new(manpage).dirname.basename).install_symlink(manpage)
          end
        end

        private

        def do_install(targets, build_isolation: true)
          args = @formula.std_pip_args(prefix: false, build_isolation: build_isolation)
          @formula.system @python, "-m", "pip", "--python=#{@venv_root}/bin/python", "install", *args, *Array(targets)
        end
      end
    end
  end
end

class PatchDSL
  attr_reader :patch_url, :patch_sha256

  def initialize
    @patch_url = nil
    @patch_sha256 = nil
  end

  attr_reader :patch_file

  def url(u, **_kwargs); @patch_url = u; end
  def sha256(s); @patch_sha256 = s; end
  def mirror(_); nil; end
  # A patch kept in the formula's repository (`Patches/...`).
  def file(path); @patch_file = path; end
  # What kind of patch it is and what it fixes: for audits only.
  def type(_); nil; end
  def resolves(*_); nil; end
end

class ResourceDSL
  attr_reader :resource_url, :resource_sha256

  def initialize(name)
    @name = name
    @resource_url = nil
    @resource_sha256 = nil
  end

  def url(u, **_kwargs); @resource_url = u; end
  def sha256(s); @resource_sha256 = s; end
  def mirror(_); nil; end
  def patch(&_block); nil; end
  def on_macos(&block); yield if OS.mac?; end
  def on_linux(&block); yield if OS.linux?; end
  def on_arm(&block); yield if Hardware::CPU.arm?; end
  def on_intel(&block); yield if Hardware::CPU.intel?; end
  # For audits and `brew livecheck` only.
  def livecheck(&_block); nil; end
  attr_reader :resource_version
  def version(value = nil); @resource_version = value.to_s if value; end
end

class StagedResource
  attr_reader :name, :url

  def initialize(url, sha256, name: nil, test: false, version: nil)
    @url = url
    @sha256 = sha256
    @name = name
    @test = test
    @version = version
  end

  # Its `version`, or as Homebrew reads one from the URL's file name.
  def version
    v = @version || File.basename(@url.to_s)[/[-_]v?(\d+(?:\.\d+)+)/, 1]
    v && FormulaVersion.new(v)
  end

  def test?; @test; end

  def stage(&block)
    Dir.mktmpdir("nb_resource_") do |root|
      basename = File.basename(URI.parse(@url).path)
      basename = "resource" if basename.empty?
      # The download apart from what it unpacks to.
      dir = File.join(root, "stage")
      FileUtils.mkdir_p([File.join(root, "download"), dir])
      archive = File.join(root, "download", basename)
      Kernel.system("curl", "-fsSL", "-o", archive, @url)
      unless $?.success?
        $stderr.puts "Error: failed to download resource #{@url}"
        exit 1
      end
      NextBrewChecksum.verify_file!(archive, @sha256, "resource #{@url}")
      # A wheel is installed as it is.
      if basename.end_with?(".whl")
        FileUtils.mv(archive, dir)
      else
        extract_resource(archive, dir)
      end
      entries = Dir.children(dir)
      src_dir = if entries.length == 1 && File.directory?(File.join(dir, entries.first))
                  File.join(dir, entries.first)
                else
                  dir
                end
      if block_given?
        Dir.chdir(src_dir) { block.call(Pathname.new(src_dir)) }
      else
        Pathname.new(src_dir)
      end
    end
  end

  private

  def extract_resource(archive, dir)
    case archive
    when /\.(tar\.gz|tgz)$/
      Kernel.system("tar", "xzf", archive, "-C", dir)
    when /\.tar\.xz$/
      Kernel.system("tar", "xJf", archive, "-C", dir)
    when /\.tar\.bz2$/
      Kernel.system("tar", "xjf", archive, "-C", dir)
    when /\.zip$/
      Kernel.system("unzip", "-qo", archive, "-d", dir)
    else
      Kernel.system("tar", "xf", archive, "-C", dir)
    end
  end
end

Resource = StagedResource unless defined?(Resource)

class BuildOptions
  def head?; false; end
  def stable?; true; end
  # Built here for this Mac, not as a bottle for others.
  def bottle?; false; end
  def with?(name); false; end
  def without?(name); true; end
end

module Metafiles
  LICENSES = %w[copying copyright license licence].freeze
  EXTENSIONS = %w[
    .adoc .asc .asciidoc .creole .html .markdown .md .mdown .mediawiki .mkdn
    .org .pod .rdoc .rst .rtf .textile .txt .wiki
  ].freeze
  BASENAMES = %w[
    about authors changelog changes history news notes notice readme todo
  ].freeze

  module_function

  def copy?(file)
    file = file.downcase
    license = file.split(/\.|-/).first
    return false unless license
    return true if LICENSES.include?(license)

    ext = File.extname(file)
    file = File.basename(file, ext) if EXTENSIONS.include?(ext)
    BASENAMES.include?(file)
  end
end

class Pathname
  # As Homebrew's, so a path goes where a string does (`ENV["X"] = prefix`).
  alias to_str to_s

  # Homebrew's: move each source here (a file it has already moved is an
  # error, as there). A directory already here gets the source's contents.
  def install(*sources)
    sources.flatten.each do |src|
      if src.is_a?(Hash)
        src.each { |from, to| install_renamed(from, to) }
        next
      end
      # A resource's contents, unpacked here (`Resource#stage(self)`).
      if src.is_a?(StagedResource)
        src.stage do |dir|
          mkpath
          dir.children.each { |child| install(child) }
        end
        next
      end
      src = Pathname.new(src) unless src.is_a?(Pathname)
      dst = self + src.basename
      if src.directory? && !src.symlink? && dst.directory?
        Dir.children(src.to_s).each { |child| dst.install(src + child) }
      else
        move_here(src, dst)
      end
    end
  end

  # Homebrew's: move the licence, readme and the like from `from` here.
  def install_metafiles(from = Pathname.pwd)
    Pathname.new(from).children.each do |p|
      next if p.directory?
      next if File.empty?(p)
      next unless Metafiles.copy?(p.basename.to_s)

      filename = p.symlink? ? p.dirname.join(p.readlink) : p
      next unless filename.exist?

      filename.chmod 0644
      install(filename)
    end
  end

  # Homebrew's: a link here to each source, relative to this directory
  # (where a relative source is read from, too).
  def install_symlink(*sources)
    sources.each do |src|
      case src
      when Array then src.each { |s| install_symlink_p(s, File.basename(s)) }
      when Hash then src.each { |s, new_basename| install_symlink_p(s, new_basename) }
      else install_symlink_p(src, File.basename(src))
      end
    end
  end

  def install_symlink_p(src, new_basename)
    mkpath
    dstdir = realpath
    src = Pathname(src).expand_path(dstdir)
    src = src.dirname.realpath + src.basename if src.dirname.exist?
    FileUtils.ln_sf(src.relative_path_from(dstdir), dstdir + new_basename.to_s)
  end

  def write(content)
    FileUtils.mkdir_p(dirname)
    File.write(to_s, content)
  end

  def unlink
    FileUtils.rm_f(to_s)
  end

  private

  def install_renamed(from, to)
    from = Pathname.new(from) unless from.is_a?(Pathname)
    move_here(from, self + to.to_s)
  end

  def move_here(src, dst)
    raise Errno::ENOENT, src.to_s if !src.symlink? && !src.exist?

    FileUtils.mkdir_p(self)
    FileUtils.mkdir_p(dst.dirname)
    # A symlink whose target has moved already: FileUtils.mv can't.
    if src.symlink?
      raise "mv #{src} #{dst} failed" unless Kernel.system("mv", src.to_s, dst.to_s)
    else
      FileUtils.mv(src.to_s, dst.to_s)
    end
  end
end

class Formula
  # As Homebrew's: `rm`, `chmod`, `cp`, `mkdir_p` and the like.
  include FileUtils

  class << self
    attr_accessor :formula_name, :formula_version

    def desc(_); nil; end
    def homepage(_); nil; end
    def license(_); nil; end
    # The stable source's URL and mirrors, for `stable.url`/`.mirrors`.
    def url(*args, **_kwargs)
      @_stable_url ||= args.first
    end
    def sha256(_); nil; end
    def revision(_); nil; end
    def compatibility_version(_); nil; end
    def mirror(url)
      (@_mirrors ||= []) << url
    end

    def stable_spec
      StableSpec.new(@_stable_url, @_mirrors || [])
    end

    # The dependencies by name, for `Language::Python` to find its Python.
    def depends_on(spec)
      name = spec.is_a?(Hash) ? spec.keys.first : spec
      (@_deps ||= []) << name.to_s if name.is_a?(String)
    end

    def dependency_names
      @_deps || []
    end
    def uses_from_macos(*_args, **_kwargs); nil; end
    def on_macos(&block); yield if OS.mac?; end
    def on_linux(&block); yield if OS.linux?; end
    def on_intel(&block); yield if Hardware::CPU.intel?; end
    def on_arm(&block); yield if Hardware::CPU.arm?; end
    def on_system(*args, **kwargs, &block)
      return unless block_given?
      if OS.linux?
        yield if args.include?(:linux)
      elsif OS.mac?
        yield if args.include?(:macos) || kwargs.key?(:macos)
      end
    end

    def head(*_args, **_kwargs, &_block); nil; end
    # The stable version's own `url`, `patch`es and `resource`s.
    def stable(&block); class_eval(&block) if block; end
    def no_autobump!(**_kwargs); nil; end
    def livecheck(&block); nil; end
    def bottle(&block); nil; end
    def skip_clean(*paths)
      skip_clean_paths.merge(paths.flatten)
    end

    def skip_clean_paths
      @skip_clean_paths ||= Set.new
    end
    def link_overwrite(*_args); nil; end
    def keg_only(*_args); nil; end
    def caveats; nil; end
    def test(&block); nil; end
    def service(&block); nil; end

    # `resource "x", :test do`: a test's resources are kept too, unused.
    def resource(name, *args, &block)
      return unless block_given?
      @_resources ||= {}
      ctx = ResourceDSL.new(name)
      ctx.instance_eval(&block)
      @_resources[name.to_s] = {
        url: ctx.resource_url, sha256: ctx.resource_sha256, test: args.include?(:test),
        version: ctx.resource_version
      }
    end

    def patch(*args, &block)
      @_patches ||= []
      strip = :p1
      data_patch = false

      args.each do |arg|
        case arg
        when :DATA
          data_patch = true
        when Symbol
          strip = arg if arg.to_s.match?(/\Ap\d+\z/)
        when String
          @_patches << { type: :inline, content: arg, strip: strip }
          return
        end
      end

      if data_patch
        @_patches << { type: :data, strip: strip }
      elsif block_given?
        ctx = PatchDSL.new
        ctx.instance_eval(&block)
        @_patches << if ctx.patch_file
          { type: :file, file: ctx.patch_file, strip: strip }
        else
          { type: :url, url: ctx.patch_url, sha256: ctx.patch_sha256, strip: strip }
        end
      end
    end

    def [](name)
      FormulaRef.new(name)
    end

    def inherited(subclass)
      subclass.formula_name = FORMULA_NAME
      subclass.formula_version = FORMULA_VERSION
      subclass.instance_variable_set(:@_patches, [])
      subclass.instance_variable_set(:@_resources, {})
    end
  end

  def name; self.class.formula_name; end
  # `version` without the revision, `pkg_version` with it (the keg's).
  def version
    FormulaVersion.new(ENV.fetch("NEXTBREW_FORMULA_STABLE_VERSION", self.class.formula_version))
  end
  def pkg_version; FormulaVersion.new(self.class.formula_version); end
  def build; BuildOptions.new; end

  def prefix
    Pathname.new(NEXTBREW_CELLAR) + name + pkg_version
  end

  def bin; prefix + "bin"; end
  def sbin; prefix + "sbin"; end
  def lib; prefix + "lib"; end
  def libexec; prefix + "libexec"; end
  def include; prefix + "include"; end
  def share; prefix + "share"; end
  def man; share + "man"; end
  def man1; man + "man1"; end
  def man2; man + "man2"; end
  def man3; man + "man3"; end
  def man4; man + "man4"; end
  def man5; man + "man5"; end
  def man6; man + "man6"; end
  def man7; man + "man7"; end
  def man8; man + "man8"; end
  def doc; share + "doc" + name; end
  def info; share + "info"; end
  def pkgshare; share + name; end
  def frameworks; prefix + "Frameworks"; end
  def kext; prefix + "Library" + "Extensions"; end

  def opt_prefix; Pathname.new(NEXTBREW_PREFIX) + "opt" + name; end
  def opt_bin; opt_prefix + "bin"; end
  def opt_sbin; opt_prefix + "sbin"; end
  def opt_lib; opt_prefix + "lib"; end
  def opt_include; opt_prefix + "include"; end
  def opt_share; opt_prefix + "share"; end
  def opt_pkgshare; opt_prefix + "share" + name; end

  # `Utils::Path::FormulaHelpers`: another formula's stable paths by name.
  def formula_opt_prefix(formula_name)
    HOMEBREW_PREFIX + "opt" + formula_name.to_s.split("/").last
  end
  def formula_opt_bin(formula_name); formula_opt_prefix(formula_name) + "bin"; end
  def formula_opt_lib(formula_name); formula_opt_prefix(formula_name) + "lib"; end
  def formula_opt_libexec(formula_name); formula_opt_prefix(formula_name) + "libexec"; end
  def formula_opt_include(formula_name); formula_opt_prefix(formula_name) + "include"; end

  # `Formula#python3`: the one `python@3.x` dependency's interpreter.
  def python3
    pythons = self.class.dependency_names.grep(/\Apython@3\.\d+\z/)
    unless pythons.length == 1
      odie "`#{name}` must have exactly one `python@3.x` dependency to use `python3`"
    end
    formula_opt_bin(pythons.first) + pythons.first.delete("@")
  end

  # `Formula#stable`: the stable source's URL, mirrors and version.
  def stable
    self.class.stable_spec.tap { |spec| spec.version = version }
  end

  # The on-system blocks inside methods, as at class level.
  def on_macos; yield if OS.mac?; end
  def on_linux; yield if OS.linux?; end
  def on_arm; yield if Hardware::CPU.arm?; end
  def on_intel; yield if Hardware::CPU.intel?; end

  # `Kernel#which`: the first executable `cmd` on `path`.
  def which(cmd, path = ENV.fetch("PATH", ""))
    path.to_s.split(File::PATH_SEPARATOR).each do |dir|
      candidate = Pathname.new(File.expand_path(cmd.to_s, dir))
      return candidate if candidate.file? && candidate.executable?
    end
    nil
  end

  # `Formula#rpath`: `target` as the dynamic loader finds it from `source`.
  def rpath(source: bin, target: lib)
    "#{loader_path}/#{target.relative_path_from(source)}"
  end

  def loader_path
    OS.mac? ? "@loader_path" : "$ORIGIN"
  end

  def bash_completion; prefix + "etc" + "bash_completion.d"; end
  def zsh_completion; share + "zsh" + "site-functions"; end
  def fish_completion; share + "fish" + "vendor_completions.d"; end
  def pwsh_completion; share + "pwsh" + "completions"; end
  def elisp; share + "emacs" + "site-lisp" + name; end

  def resource(name)
    res_info = self.class.instance_variable_get(:@_resources)&.dig(name.to_s)
    raise "Resource '#{name}' not defined" unless res_info
    StagedResource.new(res_info[:url], res_info[:sha256], name: name.to_s, test: res_info[:test],
                                                          version: res_info[:version])
  end

  # Every resource, in the order the formula declares them.
  def resources
    (self.class.instance_variable_get(:@_resources) || {}).keys.map { |name| resource(name) }
  end

  # `Formula#std_pip_args`.
  def std_pip_args(prefix: self.prefix, build_isolation: false)
    args = ["--verbose", "--no-deps", "--no-binary=:all:", "--ignore-installed", "--no-compile"]
    args << "--uploaded-prior-to=P1D"
    args << "--prefix=#{prefix}" if prefix
    args << "--no-build-isolation" unless build_isolation
    args
  end

  # `Formula#pkgetc`: the formula's own directory under the prefix's `etc`.
  def pkgetc
    HOMEBREW_PREFIX + "etc" + name
  end

  def etc
    Pathname.new(NEXTBREW_PREFIX) + "etc"
  end

  def var
    Pathname.new(NEXTBREW_PREFIX) + "var"
  end

  def buildpath
    BUILDPATH
  end

  def testpath
    buildpath + "test"
  end

  # Homebrew's `Utils::Inreplace.inreplace`: a pattern (a string or a
  # regexp) or a block's edits in each file, failing the build when one
  # changes nothing, unless `audit_result: false`.
  def inreplace(paths, before = nil, after = nil, audit_result: true, &block)
    Array(paths).flatten.each do |path|
      content = File.read(path.to_s)
      if block_given?
        s = InreplaceString.new(content)
        block.call(s)
        failed = s.failures
        content = s.to_s
      else
        replaced = content.gsub(before.is_a?(Regexp) ? before : before.to_s, after.to_s)
        failed = audit_result && replaced == content ? ["expected replacement of #{before.inspect} with #{after.inspect}"] : []
        content = replaced
      end
      odie "inreplace failed\n#{path}:\n  #{failed.join("\n  ")}" unless failed.empty?
      File.write(path.to_s, content)
    end
  end

  # Homebrew's `Formula#system`: the command as a heading (without the
  # arguments every such build passes), its output into a numbered log
  # (shown as well with `HOMEBREW_VERBOSE`), and on a failure the log's last
  # lines, before the build stops.
  def system(cmd, *args)
    pretty_args = args.dup
    case cmd.to_s
    when "./configure" then pretty_args -= std_configure_args
    when "cmake" then pretty_args -= std_cmake_args
    when "go" then pretty_args -= std_go_args
    when "meson" then pretty_args -= std_meson_args
    end
    puts "==> #{cmd} #{pretty_args * " "}".strip
    $stdout.flush

    NextBrewLogs.count += 1
    FileUtils.mkdir_p(NextBrewLogs.dir)
    log_filename = format("%<dir>s/%<count>02d.%<base>s.log", dir: NextBrewLogs.dir,
                          count: NextBrewLogs.count, base: File.basename(cmd.to_s).split.first)
    File.open(log_filename, "w") do |log|
      log.puts Time.now, "", cmd, args, ""
      log.flush
      env = { "HOMEBREW_CC_LOG_PATH" => log_filename }
      argv = args.map(&:to_s)
      status = if NextBrewLogs.verbose?
        rd, wr = IO.pipe
        pid = begin
          Process.spawn(env, cmd.to_s, *argv, [:out, :err] => wr)
        rescue SystemCallError
          nil
        end
        wr.close
        rd.each_line do |line|
          log.puts line
          puts line
        end
        rd.close
        pid && Process.wait2(pid).last
      else
        pid = begin
          Process.spawn(env, cmd.to_s, *argv, [:out, :err] => log)
        rescue SystemCallError
          nil
        end
        pid && Process.wait2(pid).last
      end
      log.puts "Failed to execute: #{cmd}" unless pid

      unless status&.success?
        log_lines = ENV.fetch("HOMEBREW_FAIL_LOG_LINES", "15").to_i
        log.flush
        unless NextBrewLogs.verbose?
          puts "Last #{log_lines} lines from #{log_filename}:"
          $stdout.flush
          Kernel.system("/usr/bin/tail", "-n", log_lines.to_s, log_filename)
        end
        NextBrewLogs.failed!
      end
    end
  end

  def mv(*sources, **options)
    normalized = sources.map { |s| s.respond_to?(:to_path) ? s.to_path : s.to_s }
    FileUtils.mv(*normalized, **options)
  end

  def std_configure_args
    ["--disable-debug", "--disable-dependency-tracking", "--prefix=#{prefix}", "--libdir=#{lib}"]
  end

  def std_cmake_args
    [
      "-DCMAKE_INSTALL_PREFIX=#{prefix}",
      "-DCMAKE_INSTALL_LIBDIR=lib",
      "-DCMAKE_BUILD_TYPE=Release",
      "-DCMAKE_FIND_FRAMEWORK=LAST",
      "-DCMAKE_VERBOSE_MAKEFILE=ON",
      "-DBUILD_TESTING=OFF",
    ]
  end

  def std_meson_args
    ["--prefix=#{prefix}", "--libdir=lib", "--buildtype=release", "--wrap-mode=nofallback"]
  end

  # Homebrew's: the arguments after `go build`, always stripping symbols.
  def std_go_args(output: bin + name, ldflags: nil, gcflags: nil, tags: nil)
    ldflags = ["-s", "-w"].concat(Array(ldflags))
    args = ["-trimpath", "-o=#{output}"]
    args << "-tags=#{Array(tags).join(",")}" if tags
    args << "-ldflags=#{ldflags.join(" ")}"
    args << "-gcflags=#{Array(gcflags).join(" ")}" if gcflags
    args
  end

  # Homebrew's: each shell's completion script, written by running
  # `commands` with that shell's parameter.
  def generate_completions_from_executable(*commands, base_name: nil, shell_parameter_format: nil,
                                           shells: nil)
    shells ||= [:cobra, :typer].include?(shell_parameter_format) ? [:bash, :zsh, :fish, :pwsh] : [:bash, :zsh, :fish]
    executable = commands.first.to_s
    base_name ||= File.basename(executable) if executable.start_with?(bin.to_s, sbin.to_s)
    base_name ||= name
    paths = {
      bash: bash_completion + base_name,
      zsh:  zsh_completion + "_#{base_name}",
      fish: fish_completion + "#{base_name}.fish",
      pwsh: pwsh_completion + "_#{base_name}.ps1",
    }
    shells.each do |shell|
      env = { "SHELL" => shell.to_s }
      shell_name = (shell == :pwsh) ? "powershell" : shell.to_s
      parameter = case shell_parameter_format
      when nil then shell_name
      when :arg then "--shell=#{shell_name}"
      when :clap
        env["COMPLETE"] = shell_name
        nil
      when :click
        env["_#{File.basename(executable).upcase.tr("-", "_")}_COMPLETE"] = "#{shell_name}_source"
        nil
      when :cobra then ["completion", shell_name]
      when :flag then "--#{shell_name}"
      when :none then nil
      when :typer
        env["_TYPER_COMPLETE_TEST_DISABLE_SHELL_DETECTION"] = "1"
        ["--show-completion", shell_name]
      else "#{shell_parameter_format}#{shell}"
      end
      args = (commands + Array(parameter)).map(&:to_s)
      output = IO.popen(env, args, &:read)
      exit 1 unless $?.success?
      paths[shell].write(output)
    end
  end

  def self.method_missing(method_name, *args, &block)
    nil
  end

  def self.respond_to_missing?(method_name, include_private = false)
    true
  end
end

class FormulaRef
  def initialize(name)
    @name = name.to_s
  end

  def opt_prefix
    dep_info = INSTALLED_DEPS[@name]
    return Pathname.new(NEXTBREW_PREFIX) + "opt" + @name if dep_info
    Pathname.new(NEXTBREW_PREFIX) + "opt" + @name
  end

  def opt_lib; opt_prefix + "lib"; end
  def opt_include; opt_prefix + "include"; end
  def opt_bin; opt_prefix + "bin"; end
  def lib; opt_lib; end
  def include; opt_include; end

  def prefix
    dep_info = INSTALLED_DEPS[@name]
    return Pathname.new(dep_info["cellar_path"]) if dep_info
    opt_prefix
  end

  def any_installed_version; self; end

  def to_s; @name; end
end

class InreplaceString
  attr_reader :failures

  def initialize(content)
    @content = content
    @failures = []
  end

  # Homebrew's: each edit is checked to have changed something, unless
  # `audit_result: false`.
  def gsub!(pattern, replacement = nil, legacy_audit = true, audit_result: legacy_audit, &block)
    edit(:gsub, pattern, replacement, audit_result, &block)
  end

  def sub!(pattern, replacement = nil, legacy_audit = true, audit_result: legacy_audit, &block)
    edit(:sub, pattern, replacement, audit_result, &block)
  end

  def edit(how, pattern, replacement, audit_result, &block)
    result = block ? @content.public_send(how, pattern, &block) : @content.public_send(how, pattern, replacement)
    if audit_result && result == @content
      @failures << "expected replacement of #{pattern.inspect} with #{replacement.inspect}"
    end
    @content = result
  end

  def change_make_var!(name, value)
    @content = @content.gsub(/^#{Regexp.escape(name)}\s*[?:]?=.*$/, "#{name}=#{value}")
  end

  def remove_make_var!(name)
    @content = @content.gsub(/^#{Regexp.escape(name)}\s*[?:]?=.*$\n?/, "")
  end

  def to_s; @content; end
end

def shared_library(name, version = nil)
  if OS.mac?
    version ? "#{name}.#{version}.dylib" : "#{name}.dylib"
  else
    version ? "#{name}.so.#{version}" : "#{name}.so"
  end
end

# Homebrew's `Cleaner` (with the macOS executable test), run on the keg as
# `FormulaInstaller#clean` does after the build: libtool and Perl files out,
# fixed permissions, conflicting info `dir` files and empty directories
# and dangling links gone, Python metadata that records the build made
# reproducible. Paths the formula names in `skip_clean` are left alone.
class NextBrewCleaner
  PERL_BASENAMES = Set.new(%w[perllocal.pod .packlist]).freeze

  def initialize(formula)
    @formula = formula
    @prefix = formula.prefix
    @paths = formula.class.skip_clean_paths
  end

  def clean
    charset = @formula.lib + "charset.alias"
    charset.unlink if charset.exist?
    [@formula.bin, @formula.sbin, @formula.lib].each { |dir| clean_dir(dir) if dir.exist? }
    if @formula.info.exist?
      @formula.info.glob("**/dir").each do |file|
        next unless file.file?
        next if file == @formula.info + @formula.name + "dir"
        next if skip_clean?(file)

        file.unlink
      end
    end
    clean_python_metadata
    prune
  end

  private

  # `Formula#skip_clean?`.
  def skip_clean?(path)
    return true if path.extname == ".la" && @paths.include?(:la)

    @paths.include?(path.relative_path_from(@prefix).to_s)
  end

  def clean_dir(directory)
    directory.find do |path|
      Find.prune if skip_clean?(path)
      next if path.directory?

      if path.extname == ".la" || path.extname == ".tbd" || PERL_BASENAMES.include?(path.basename.to_s)
        path.unlink
      elsif !path.symlink?
        path.chmod(executable_path?(path) ? 0o555 : 0o444)
      end
    end
  end

  # `Utils::Path.text_executable?`, or a Mach-O executable.
  def executable_path?(path)
    head = File.open(path, "rb") { |f| f.read(4) } || ""
    return true if head.start_with?("#!")

    magic = head.unpack1("N")
    return false unless [0xfeedface, 0xfeedfacf, 0xcefaedfe, 0xcffaedfe, 0xcafebabe].include?(magic)

    out = IO.popen(["file", "-b", path.to_s], err: File::NULL, &:read).to_s
    out.include?("executable")
  rescue SystemCallError
    path.executable?
  end

  def clean_python_metadata
    @prefix.realpath.find do |path|
      Find.prune if skip_clean?(path)
      next if path.directory? || path.symlink?
      next if path.parent.extname != ".dist-info"

      case path.basename.to_s
      when "direct_url.json", "RECORD" then path.unlink
      when "INSTALLER" then File.write(path, "brew\n")
      end
    end
  end

  def prune
    dirs = []
    symlinks = []
    @prefix.find do |path|
      if path == @formula.libexec || skip_clean?(path)
        Find.prune
      elsif path.symlink?
        symlinks << path
      elsif path.directory?
        dirs << path
      end
    end
    dirs.reverse_each { |d| d.rmdir if d.children.empty? }
    symlinks.reverse_each { |s| s.unlink unless File.exist?(s) }
  end
end

formula_raw = File.read(FORMULA_FILE)
end_marker_idx = formula_raw.index(/^__END__\s*$/)
FORMULA_DATA_CONTENT = end_marker_idx ? formula_raw[(formula_raw.index("\n", end_marker_idx) + 1)..] : nil

ENV["HOMEBREW_PREFIX"] = NEXTBREW_PREFIX
ENV["HOMEBREW_CELLAR"] = NEXTBREW_CELLAR

load FORMULA_FILE

formula_class = ObjectSpace.each_object(Class).find { |c| c < Formula && c != Formula }
unless formula_class
  $stderr.puts "Error: no formula class found in #{FORMULA_FILE}"
  exit 1
end

# `patch` on a file it can seek in, as some patches need.
def nextbrew_patch(strip_flag, content)
  Tempfile.create("nb_patch") do |tmp|
    tmp.write(content)
    tmp.close
    Kernel.system("patch", strip_flag, "-i", tmp.path)
  end
end

patches = formula_class.instance_variable_get(:@_patches) || []
# Fetch the patches' files together first, as Homebrew does before the
# build, then apply them in order.
fetched = patches.each_with_index.select { |p, _| p[:type] == :url }.map do |p, i|
  tmp = Tempfile.new("nb_patch")
  tmp.close
  [i, tmp, Thread.new { Kernel.system("curl", "-sSL", "-o", tmp.path, p[:url]) }]
end
downloads = fetched.to_h { |i, tmp, thread| [i, [tmp, thread.value]] }
patches.each_with_index do |p, index|
  strip_flag = "-#{p[:strip]}"
  case p[:type]
  when :data
    if FORMULA_DATA_CONTENT
      puts "==> Applying DATA patch"
      nextbrew_patch(strip_flag, FORMULA_DATA_CONTENT)
      unless $?.success?
        $stderr.puts "Error: DATA patch failed"
        exit 1
      end
    end
  when :url
    puts "==> Applying patch #{p[:url]}"
    tmp, downloaded = downloads.fetch(index)
    begin
      unless downloaded
        $stderr.puts "Error: failed to download patch #{p[:url]}"
        exit 1
      end
      NextBrewChecksum.verify_file!(tmp.path, p[:sha256], "patch #{p[:url]}")
      Kernel.system("patch", strip_flag, "-i", tmp.path)
      unless $?.success?
        $stderr.puts "Error: patch failed"
        exit 1
      end
    ensure
      tmp.close!
    end
  when :file
    # From the repository the formula came from: a tap's checkout, or
    # homebrew-core's files online.
    base = ENV.fetch("NEXTBREW_PATCH_BASE", "")
    puts "==> Applying #{p[:file]}"
    content, fetched = if base.start_with?("http")
      [IO.popen(["curl", "-fsSL", "#{base}/#{p[:file]}"], &:read), $?.success?]
    else
      path = File.join(base, p[:file])
      File.file?(path) ? [File.read(path), true] : ["", false]
    end
    unless fetched
      $stderr.puts "Error: failed to fetch #{p[:file]}"
      exit 1
    end
    nextbrew_patch(strip_flag, content)
    unless $?.success?
      $stderr.puts "Error: patch #{p[:file]} failed"
      exit 1
    end
  when :inline
    puts "==> Applying inline patch"
    nextbrew_patch(strip_flag, p[:content])
    unless $?.success?
      $stderr.puts "Error: inline patch failed"
      exit 1
    end
  end
end

instance = formula_class.new

puts "==> Building #{FORMULA_NAME} #{FORMULA_VERSION}"
FileUtils.mkdir_p(instance.prefix.to_s)
instance.install

# As Homebrew's build: the licence and readme into the keg, from the
# source, or else from `libexec`.
instance.prefix.install_metafiles(BUILDPATH)
if instance.libexec.exist? &&
   instance.prefix.children.none? { |p| p.file? && Metafiles.copy?(p.basename.to_s) }
  instance.prefix.install_metafiles(instance.libexec)
end
NextBrewCleaner.new(instance).clean
puts "==> Build complete: #{FORMULA_NAME} #{FORMULA_VERSION}"
