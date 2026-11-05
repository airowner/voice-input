-- Simulation only: validates installed Omarchy helper, never calls compositor.
local observed
hl = {
  dsp = { exec_cmd = function(command) return command end },
  bind = function(keys, command, options)
    observed = { keys = keys, command = command, release = options.release }
  end,
}
o = {}
assert(loadfile('/usr/share/omarchy/default/hypr/helpers.lua'))()
local fragment = [[
o.bind('SUPER + SHIFT + D', 'Voice input', '/usr/bin/voice-inputctl toggle', { release = true })
]]
assert(load(fragment, 'voice-input-binding'))()
assert(observed.keys == 'SUPER + SHIFT + D')
assert(observed.command == '/usr/bin/voice-inputctl toggle')
assert(observed.release == true)
print('PASS (simulation): installed Omarchy helper forwards release=true and command')
print('NOT VERIFIED: live registration, shortcut conflicts, physical key release')
