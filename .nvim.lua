-- Project-local rust-analyzer config (sourced via :exrc; run :trust on first open).
--
-- The /firmware crate cross-compiles to xtensa-esp32-espidf with Espressif's
-- `esp` toolchain and needs the esp build env; analysing it with the host target
-- fills the buffer with bogus errors. core/ and the charge/ daemon are plain host
-- crates and keep their normal (global) rustaceanvim settings — rustaceanvim
-- roots a separate rust-analyzer per crate, so we only override the firmware one.

local repo = vim.fs.normalize(vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h"))
local firmware_root = repo .. "/firmware"

-- Read LIBCLANG_PATH + the xtensa-gcc PATH entry straight from the file espup
-- maintains, so a toolchain version bump doesn't break this config.
local function esp_env()
  local env = {}
  local f = io.open(vim.fn.expand("~/export-esp.sh"), "r")
  if f then
    for line in f:lines() do
      local k, v = line:match('^export%s+([%w_]+)="(.-)"')
      if k == "LIBCLANG_PATH" then
        env.LIBCLANG_PATH = v
      elseif k == "PATH" then
        env.PATH = (v:gsub("%$PATH", function() return vim.env.PATH or "" end))
      end
    end
    f:close()
  end
  -- esp-clang needs the libxml2/ICU compat shim on rolling distros (harmless if
  -- the dir is absent elsewhere). WiFi/MQTT are placeholders so env!() resolves
  -- in rust-analyzer without leaking real creds.
  local compat = vim.fn.expand("~/.espressif/compat-libs")
  local ld = vim.env.LD_LIBRARY_PATH
  env.LD_LIBRARY_PATH = ld and (compat .. ":" .. ld) or compat
  env.WIFI_SSID = "placeholder"
  env.WIFI_PASSWORD = "placeholder"
  env.MQTT_URL = "mqtt://localhost:1883"
  return env
end

local function firmware_settings()
  local e = esp_env()
  return {
    ["rust-analyzer"] = {
      cargo = {
        target = "xtensa-esp32-espidf",
        extraEnv = e,
        allFeatures = false,
        buildScripts = { enable = true },
      },
      check = { extraEnv = e, allTargets = false },
      procMacro = { enable = true },
    },
  }
end

-- Capture the global settings table so non-firmware crates keep their behaviour,
-- then swap settings for a per-root function. Deferred to VeryLazy because the
-- rustaceanvim plugin config (which sets vim.g.rustaceanvim) may run after exrc.
local base_settings = {}

local function install()
  local g = vim.g.rustaceanvim or {}
  g.server = g.server or {}
  if type(g.server.settings) == "table" then
    base_settings = g.server.settings
  end
  g.server.settings = function(project_root)
    if project_root and vim.fs.normalize(project_root) == firmware_root then
      return vim.tbl_deep_extend("force", vim.deepcopy(base_settings), firmware_settings())
    end
    return base_settings
  end
  vim.g.rustaceanvim = g
end

if type(vim.g.rustaceanvim) == "table" and vim.g.rustaceanvim.server and type(vim.g.rustaceanvim.server.settings) == "table" then
  install()
else
  vim.api.nvim_create_autocmd("User", { pattern = "VeryLazy", once = true, callback = install })
end

-- If a firmware buffer was already open before this file was trusted, run
-- :LspRestart once so the client picks up the xtensa target.
