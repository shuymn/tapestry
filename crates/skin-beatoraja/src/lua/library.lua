-- The sandbox's library (library.rs): the base, string, table and math
-- functions a skin may call, the allowlist the owner's skins use. Every
-- loop whose length a skin chooses runs here, as Lua, so the instruction
-- hook meters it and sees cancellation. A native function this chunk
-- keeps is its private upvalue and is called only with arguments of the
-- exact Lua type it takes, checked first, never coerced (a string where
-- a number belongs, or the reverse, is an error), and only for a result
-- whose size is known and charged first (bytes). An unsupported form is
-- refused (refuse), never approximated.
local native, raw_charge, raw_refuse, bridge, cap, max_path, fault = ...

local byte, sub = native.byte, native.sub
local native_char, native_upper, native_lower = native.char, native.upper, native.lower
local native_format, native_concat = native.format, native.concat
local native_tonumber, native_tostring = native.tonumber, native.tostring
local native_setmetatable, native_getmetatable = native.setmetatable, native.getmetatable
local native_select, native_error = native.select, native.error
local native_floor, native_ceil, native_abs = native.floor, native.ceil, native.abs
local tointeger, mathtype = native.tointeger, native.mathtype
local type, next = native.type, native.next
local rawget, rawset, rawlen = native.rawget, native.rawset, native.rawlen

-- The host's primitives return their status; a fault is raised here, in
-- Lua, as the fixed marker `fault`.
local function charge(n)
  if not raw_charge(n) then
    native_error(fault, 0)
  end
end

local function refuse(code)
  raw_refuse(code)
  native_error(fault, 0)
end
local error, select = native_error, native_select

-- The refusal codes library.rs names.
local PATTERN_BALANCE, PATTERN_FRONTIER, PATTERN_BACKREF = 1, 2, 3
local METATABLE, TOO_LARGE, FORMAT, PATTERN_ZERO = 4, 5, 6, 7
local FINALIZER, TONUMBER, NAME, PATH = 0, 8, 9, 10

-- The most values string.char takes in one call.
local MAX_VALUES = 4096
-- The most results string.byte gives in one call: what fits in the 20
-- free stack slots Lua gives every native call (LUA_MINSTACK), so the
-- native never grows the stack and cannot fail to.
local MAX_RESULTS = 16
-- The most conversions one string.format takes.
local MAX_CONVERSIONS = 32
-- The longest `__name` a metatable may hold: Lua copies it into every
-- `tostring` of the table and every type error naming it.
local MAX_NAME = 64

-- The values t[i] to t[n], returned by Lua itself: the VM grows its
-- stack for them, and an allocation failure there is fatal, where a
-- native `unpack` would raise an ordinary error. Each return moves the
-- values after it, so n stays small (at most 32 here).
local function spread(t, i, n)
  if i > n then
    return
  end
  return t[i], spread(t, i + 1, n)
end

local function arg_error(n, fname, msg)
  native_error("bad argument #" .. n .. " to '" .. fname .. "' (" .. msg .. ")", 3)
end

local function got(v)
  return v == nil and "no value" or type(v)
end

-- Argument n, which must be a string.
local function str(v, n, fname)
  if type(v) ~= "string" then
    arg_error(n, fname, "string expected, got " .. got(v))
  end
  return v
end

-- Argument n, which must be an integer, or a float with an integral
-- value; never a string.
local function int(v, n, fname, default)
  if v == nil and default ~= nil then
    return default
  end
  if type(v) ~= "number" then
    arg_error(n, fname, "number expected, got " .. got(v))
  end
  local i = tointeger(v)
  if i == nil then
    arg_error(n, fname, "number has no integer representation")
  end
  return i
end

-- Argument n, which must be a number.
local function num(v, n, fname)
  if type(v) ~= "number" then
    arg_error(n, fname, "number expected, got " .. got(v))
  end
  return v
end

-- A table these functions read raw: one with a metatable, which Lua's
-- own functions would consult, is refused.
local function plain(t, n, fname)
  if type(t) ~= "table" then
    arg_error(n, fname, "table expected, got " .. got(t))
  end
  if native_getmetatable(t) ~= nil then
    refuse(METATABLE)
  end
end

-- Charges n bytes a native call allocates, refusing a result past the
-- per-call cap.
local function bytes(n)
  if n > cap then
    refuse(TOO_LARGE)
  end
  if n > 0 then
    charge(n)
  end
end

-- Lua's posrelatI: a start position, 1 for anything below the string.
local function start_of(pos, l)
  if pos > 0 then
    return pos
  elseif pos == 0 then
    return 1
  elseif pos < -l then
    return 1
  end
  return l + pos + 1
end

-- Lua's getendpos: an end position, clamped to the string.
local function end_of(pos, l)
  if pos > l then
    return l
  elseif pos >= 0 then
    return pos
  elseif pos < -l then
    return 0
  end
  return l + pos + 1
end

---------------------------------------------------------------------------
-- string

local string_lib = {}

function string_lib.len(s)
  return #str(s, 1, "len")
end

function string_lib.sub(s, i, j)
  str(s, 1, "sub")
  local l = #s
  i = start_of(int(i, 2, "sub", 1), l)
  j = end_of(int(j, 3, "sub", -1), l)
  if i <= j then
    bytes(j - i + 1)
  end
  return sub(s, i, j)
end

function string_lib.upper(s)
  bytes(#str(s, 1, "upper"))
  return native_upper(s)
end

function string_lib.lower(s)
  bytes(#str(s, 1, "lower"))
  return native_lower(s)
end

function string_lib.byte(s, i, j)
  str(s, 1, "byte")
  local l = #s
  local pi = int(i, 2, "byte", 1)
  i = start_of(pi, l)
  j = end_of(int(j, 3, "byte", pi), l)
  if i > j then
    return
  end
  if j - i + 1 > MAX_RESULTS then
    refuse(TOO_LARGE)
  end
  return byte(s, i, j)
end

function string_lib.char(...)
  local n = native_select("#", ...)
  if n > MAX_VALUES then
    refuse(TOO_LARGE)
  end
  local args = { ... }
  for k = 1, n do
    local c = args[k]
    if mathtype(c) ~= "integer" then
      arg_error(k, "char", "integer expected, got " .. (mathtype(c) or got(c)))
    end
    if c < 0 or c > 255 then
      arg_error(k, "char", "value out of range")
    end
  end
  bytes(n)
  return native_char(...)
end

-- string.format's conversions the library takes: `d` (an integer), `f`
-- (a number) and `s` (any value, by its tostring), with the flags `-`
-- and `0`, a width and a precision of at most two digits each, and
-- `%%`. Any other is refused.
function string_lib.format(fmt, ...)
  str(fmt, 1, "format")
  local args = { ... }
  local nargs = native_select("#", ...)
  local l = #fmt
  local total, scan, argi, i = 0, 0, 0, 1
  while i <= l do
    local c = byte(fmt, i)
    if c ~= 37 then
      total = total + 1
      i = i + 1
    elseif byte(fmt, i + 1) == 37 then
      total = total + 1
      i = i + 2
    else
      i = i + 1
      while byte(fmt, i) == 45 or byte(fmt, i) == 48 do
        i = i + 1
      end
      local width, digits = 0, 0
      while (byte(fmt, i) or 0) >= 48 and (byte(fmt, i) or 0) <= 57 do
        width = width * 10 + (byte(fmt, i) - 48)
        digits = digits + 1
        i = i + 1
      end
      local precision, pdigits = nil, 0
      if byte(fmt, i) == 46 then
        precision = 0
        i = i + 1
        while (byte(fmt, i) or 0) >= 48 and (byte(fmt, i) or 0) <= 57 do
          precision = precision * 10 + (byte(fmt, i) - 48)
          pdigits = pdigits + 1
          i = i + 1
        end
      end
      if digits > 2 or pdigits > 2 then
        refuse(FORMAT)
      end
      local letter = byte(fmt, i)
      i = i + 1
      argi = argi + 1
      if argi > MAX_CONVERSIONS then
        refuse(FORMAT)
      end
      if argi > nargs then
        arg_error(argi + 1, "format", "no value")
      end
      local v = args[argi]
      if letter == 100 then -- d
        if precision ~= nil then
          refuse(FORMAT)
        end
        args[argi] = int(v, argi + 1, "format")
        total = total + width + 20
      elseif letter == 102 then -- f
        num(v, argi + 1, "format")
        total = total + width + (precision or 6) + 320
      elseif letter == 115 then -- s
        if type(v) ~= "string" then
          v = native_tostring(v)
          args[argi] = v
        end
        local n = #v
        -- The native reads the whole string (it checks for zeros) even
        -- when a precision keeps less of it.
        scan = scan + n
        if precision ~= nil and precision < n then
          n = precision
        end
        total = total + (width > n and width or n)
      else
        refuse(FORMAT)
      end
      if total > cap then
        refuse(TOO_LARGE)
      end
    end
  end
  bytes(total)
  if scan > total then
    charge(scan - total)
  end
  return native_format(fmt, spread(args, 1, argi))
end

---------------------------------------------------------------------------
-- patterns: one matcher, Lua 5.4's (lstrlib.c), for find, match and
-- gsub. %b, %f, %z and back references are refused.

local MAXCAPTURES, MAXDEPTH = 32, 200
local CAP_UNFINISHED, CAP_POSITION = -1, -2

local function is_alpha(c) return (c >= 65 and c <= 90) or (c >= 97 and c <= 122) end
local function is_digit(c) return c >= 48 and c <= 57 end
local function is_lower(c) return c >= 97 and c <= 122 end
local function is_upper(c) return c >= 65 and c <= 90 end
local function is_space(c) return c == 32 or (c >= 9 and c <= 13) end
local function is_cntrl(c) return c < 32 or c == 127 end
local function is_graph(c) return c >= 33 and c <= 126 end
local function is_alnum(c) return is_alpha(c) or is_digit(c) end
local function is_punct(c) return is_graph(c) and not is_alnum(c) end
local function is_xdigit(c)
  return is_digit(c) or (c >= 97 and c <= 102) or (c >= 65 and c <= 70)
end

local CLASSES = { [97] = is_alpha, [99] = is_cntrl, [100] = is_digit, [103] = is_graph,
  [108] = is_lower, [112] = is_punct, [115] = is_space, [117] = is_upper, [119] = is_alnum,
  [120] = is_xdigit }

local function match_class(c, cl)
  local lower = cl
  if is_upper(cl) then
    lower = cl + 32
  end
  if lower == 122 then
    refuse(PATTERN_ZERO)
  end
  local f = CLASSES[lower]
  if not f then
    return cl == c
  end
  local r = f(c)
  if is_upper(cl) then
    return not r
  end
  return r
end

-- The match state: the subject, the pattern and the captures.
local M = {}

local function class_end(pi)
  local p, pl = M.p, M.pl
  local c = byte(p, pi)
  pi = pi + 1
  if c == 37 then
    if pi > pl then
      error("malformed pattern (ends with '%')", 0)
    end
    return pi + 1
  elseif c == 91 then
    if byte(p, pi) == 94 then
      pi = pi + 1
    end
    repeat
      if pi > pl then
        error("malformed pattern (missing ']')", 0)
      end
      local cc = byte(p, pi)
      pi = pi + 1
      if cc == 37 and pi <= pl then
        pi = pi + 1
      end
    until byte(p, pi) == 93
    return pi + 1
  end
  return pi
end

local function match_bracket(c, pi, ec)
  local p = M.p
  local sig = true
  if byte(p, pi + 1) == 94 then
    sig = false
    pi = pi + 1
  end
  pi = pi + 1
  while pi < ec do
    local pc = byte(p, pi)
    if pc == 37 then
      pi = pi + 1
      if match_class(c, byte(p, pi)) then
        return sig
      end
    elseif byte(p, pi + 1) == 45 and pi + 2 < ec then
      if pc <= c and c <= byte(p, pi + 2) then
        return sig
      end
      pi = pi + 2
    elseif pc == c then
      return sig
    end
    pi = pi + 1
  end
  return not sig
end

local function single(si, pi, ep)
  if si > M.sl then
    return false
  end
  local c = byte(M.s, si)
  local pc = byte(M.p, pi)
  if pc == 46 then
    return true
  elseif pc == 37 then
    return match_class(c, byte(M.p, pi + 1))
  elseif pc == 91 then
    return match_bracket(c, pi, ep - 1)
  end
  return pc == c
end

local do_match

local function max_expand(si, pi, ep)
  local i = 0
  while single(si + i, pi, ep) do
    i = i + 1
  end
  while i >= 0 do
    local r = do_match(si + i, ep + 1)
    if r then
      return r
    end
    i = i - 1
  end
  return nil
end

local function min_expand(si, pi, ep)
  while true do
    local r = do_match(si, ep + 1)
    if r then
      return r
    elseif single(si, pi, ep) then
      si = si + 1
    else
      return nil
    end
  end
end

local function start_capture(si, pi, what)
  local level = M.level
  if level >= MAXCAPTURES then
    error("too many captures", 0)
  end
  M.init[level + 1] = si
  M.len[level + 1] = what
  M.level = level + 1
  local r = do_match(si, pi)
  if not r then
    M.level = M.level - 1
  end
  return r
end

local function end_capture(si, pi)
  local l = nil
  for k = M.level, 1, -1 do
    if M.len[k] == CAP_UNFINISHED then
      l = k
      break
    end
  end
  if not l then
    error("invalid pattern capture", 0)
  end
  M.len[l] = si - M.init[l]
  local r = do_match(si, pi)
  if not r then
    M.len[l] = CAP_UNFINISHED
  end
  return r
end

function do_match(si, pi)
  M.depth = M.depth - 1
  if M.depth == 0 then
    error("pattern too complex", 0)
  end
  local p, pl = M.p, M.pl
  while true do
    if pi > pl then
      break
    end
    local pc = byte(p, pi)
    if pc == 40 then
      if byte(p, pi + 1) == 41 then
        si = start_capture(si, pi + 2, CAP_POSITION)
      else
        si = start_capture(si, pi + 1, CAP_UNFINISHED)
      end
      break
    elseif pc == 41 then
      si = end_capture(si, pi + 1)
      break
    elseif pc == 36 and pi == pl then
      if si ~= M.sl + 1 then
        si = nil
      end
      break
    elseif pc == 37 and byte(p, pi + 1) == 98 then
      refuse(PATTERN_BALANCE)
    elseif pc == 37 and byte(p, pi + 1) == 102 then
      refuse(PATTERN_FRONTIER)
    elseif pc == 37 and is_digit(byte(p, pi + 1) or 0) then
      refuse(PATTERN_BACKREF)
    else
      local ep = class_end(pi)
      local epc = byte(p, ep)
      if not single(si, pi, ep) then
        if epc == 42 or epc == 63 or epc == 45 then
          pi = ep + 1
        else
          si = nil
          break
        end
      else
        if epc == 63 then
          local r = do_match(si + 1, ep + 1)
          if r then
            si = r
            break
          end
          pi = ep + 1
        elseif epc == 43 then
          si = max_expand(si + 1, pi, ep)
          break
        elseif epc == 42 then
          si = max_expand(si, pi, ep)
          break
        elseif epc == 45 then
          si = min_expand(si, pi, ep)
          break
        else
          si = si + 1
          pi = ep
        end
      end
    end
  end
  M.depth = M.depth + 1
  return si
end

-- A fresh state for each matching call, so that a replacement function
-- that matches too leaves the outer call's state alone.
local function prepare(s, p)
  M = { s = s, sl = #s, p = p, pl = #p, init = {}, len = {} }
end

local function reset()
  M.level = 0
  M.depth = MAXDEPTH
end

-- Capture i (1-based) of a match from si to just before e; the whole
-- match when there are none.
local function capture(i, si, e)
  if i > M.level then
    if i == 1 then
      bytes(e - si)
      return sub(M.s, si, e - 1)
    end
    error("invalid capture index %" .. i, 0)
  end
  local l = M.len[i]
  if l == CAP_UNFINISHED then
    error("unfinished capture", 0)
  elseif l == CAP_POSITION then
    return M.init[i]
  end
  bytes(l)
  return sub(M.s, M.init[i], M.init[i] + l - 1)
end

local function captures(si, e, whole)
  local n = M.level
  if n == 0 and whole then
    n = 1
  end
  local out = {}
  for i = 1, n do
    out[i] = capture(i, si, e)
  end
  return spread(out, 1, n)
end

-- Whether a pattern has none of the special characters.
local function no_specials(p)
  for i = 1, #p do
    local c = byte(p, i)
    if c == 94 or c == 36 or c == 42 or c == 43 or c == 63 or c == 46 or c == 40
      or c == 41 or c == 91 or c == 93 or c == 37 or c == 45 then
      return false
    end
  end
  return true
end

local function plain_find(s, p, init)
  local sl, pl = #s, #p
  if pl == 0 then
    return init, init - 1
  end
  local first = byte(p, 1)
  for i = init, sl - pl + 1 do
    if byte(s, i) == first then
      local k = 2
      while k <= pl and byte(s, i + k - 1) == byte(p, k) do
        k = k + 1
      end
      if k > pl then
        return i, i + pl - 1
      end
    end
  end
  return nil
end

local function find_aux(s, p, init, plain_arg, find, fname)
  s = str(s, 1, fname)
  p = str(p, 2, fname)
  local sl = #s
  init = start_of(int(init, 3, fname, 1), sl)
  if init > sl + 1 then
    return nil
  end
  if find and (plain_arg or no_specials(p)) then
    return plain_find(s, p, init)
  end
  prepare(s, p)
  local pi = 1
  local anchor = byte(p, 1) == 94
  if anchor then
    pi = 2
  end
  local si = init
  repeat
    reset()
    local e = do_match(si, pi)
    if e then
      if find then
        return si, e - 1, captures(si, e, false)
      end
      return captures(si, e, true)
    end
    si = si + 1
  until si > sl + 1 or anchor
  return nil
end

function string_lib.find(s, p, init, plain_arg)
  return find_aux(s, p, init, plain_arg, true, "find")
end

function string_lib.match(s, p, init)
  return find_aux(s, p, init, false, false, "match")
end


-- The text of a gsub replacement for a match from si to e.
local function replacement(repl, kind, si, e, out)
  if kind == "string" then
    local rl = #repl
    local i = 1
    while i <= rl do
      local c = byte(repl, i)
      if c == 37 then
        i = i + 1
        local d = byte(repl, i)
        if d == 37 then
          out[#out + 1] = "%"
        elseif d and is_digit(d) then
          local v
          if d == 48 then
            bytes(e - si)
            v = sub(M.s, si, e - 1)
          else
            v = capture(d - 48, si, e)
          end
          if type(v) == "number" then
            v = native_tostring(v)
          end
          out[#out + 1] = v
        else
          error("invalid use of '%' in replacement string", 0)
        end
        i = i + 1
      else
        local j = i
        while j <= rl and byte(repl, j) ~= 37 do
          j = j + 1
        end
        bytes(j - i)
        out[#out + 1] = sub(repl, i, j - 1)
        i = j
      end
    end
    return
  end
  local key = capture(1, si, e)
  local v
  if kind == "table" then
    v = rawget(repl, key)
  else
    local outer = M
    v = repl(captures(si, e, true))
    M = outer
  end
  if v == nil or v == false then
    bytes(e - si)
    out[#out + 1] = sub(M.s, si, e - 1)
  elseif type(v) == "string" or type(v) == "number" then
    out[#out + 1] = type(v) == "number" and native_tostring(v) or v
  else
    error("invalid replacement value (a " .. type(v) .. ")", 0)
  end
end

local function join(out, sep)
  local total = 0
  for k = 1, #out do
    total = total + #out[k]
  end
  bytes(total + #sep * (#out > 0 and #out - 1 or 0))
  -- The native visits every entry, an empty one too.
  charge(#out)
  return native_concat(out, sep)
end

function string_lib.gsub(s, p, repl, max_n)
  s = str(s, 1, "gsub")
  p = str(p, 2, "gsub")
  local kind = type(repl)
  if kind == "table" then
    plain(repl, 3, "gsub")
  elseif kind ~= "string" and kind ~= "function" then
    arg_error(3, "gsub", "string/function/table expected, got " .. kind)
  end
  local sl = #s
  max_n = int(max_n, 4, "gsub", sl + 1)
  prepare(s, p)
  local pi = 1
  local anchor = byte(p, 1) == 94
  if anchor then
    pi = 2
  end
  local out = {}
  local si, n, last, copied = 1, 0, nil, 1
  while n < max_n do
    reset()
    local e = do_match(si, pi)
    if e and e ~= last then
      n = n + 1
      if si > copied then
        bytes(si - copied)
        out[#out + 1] = sub(s, copied, si - 1)
      end
      replacement(repl, kind, si, e, out)
      si, last, copied = e, e, e
    elseif si <= sl then
      si = si + 1
    else
      break
    end
    if anchor then
      break
    end
  end
  if copied <= sl then
    bytes(sl - copied + 1)
    out[#out + 1] = sub(s, copied, sl)
  end
  return join(out, ""), n
end

---------------------------------------------------------------------------
-- table: raw access only

local table_lib = {}

function table_lib.insert(t, ...)
  plain(t, 1, "insert")
  local n = select("#", ...)
  local e = rawlen(t) + 1
  if n == 1 then
    rawset(t, e, ...)
    return
  elseif n ~= 2 then
    error("wrong number of arguments to 'insert'", 2)
  end
  local pos, v = ...
  pos = int(pos, 2, "insert")
  if pos < 1 or pos > e then
    arg_error(2, "insert", "position out of bounds")
  end
  for i = e, pos + 1, -1 do
    rawset(t, i, rawget(t, i - 1))
  end
  rawset(t, pos, v)
end

function table_lib.remove(t, pos)
  plain(t, 1, "remove")
  local size = rawlen(t)
  pos = int(pos, 2, "remove", size)
  if pos ~= size and (pos < 1 or pos > size + 1) then
    arg_error(2, "remove", "position out of bounds")
  end
  local v = rawget(t, pos)
  while pos < size do
    rawset(t, pos, rawget(t, pos + 1))
    pos = pos + 1
  end
  rawset(t, pos, nil)
  return v
end

function table_lib.concat(t, sep, i, j)
  plain(t, 1, "concat")
  sep = sep == nil and "" or str(sep, 2, "concat")
  i = int(i, 3, "concat", 1)
  j = int(j, 4, "concat", rawlen(t))
  local out = {}
  for k = i, j do
    local v = rawget(t, k)
    local tv = type(v)
    if tv == "number" then
      v = native_tostring(v)
    elseif tv ~= "string" then
      error("invalid value (at index " .. k .. ") in table for 'concat'", 2)
    end
    out[#out + 1] = v
  end
  return join(out, sep)
end

-- A merge sort: stable, its comparisons the skin's comparator or `<`.
function table_lib.sort(t, comp)
  plain(t, 1, "sort")
  if comp ~= nil and type(comp) ~= "function" then
    arg_error(2, "sort", "function expected, got " .. type(comp))
  end
  local n = rawlen(t)
  local less = comp or function(a, b) return a < b end
  local a, b = {}, {}
  for k = 1, n do
    a[k] = rawget(t, k)
  end
  local width = 1
  while width < n do
    local lo = 1
    while lo <= n do
      local mid = lo + width
      local hi = lo + 2 * width
      if mid > n + 1 then mid = n + 1 end
      if hi > n + 1 then hi = n + 1 end
      local x, y, k = lo, mid, lo
      while x < mid and y < hi do
        if less(a[y], a[x]) then
          b[k] = a[y]
          y = y + 1
        else
          b[k] = a[x]
          x = x + 1
        end
        k = k + 1
      end
      while x < mid do
        b[k] = a[x]
        x, k = x + 1, k + 1
      end
      while y < hi do
        b[k] = a[y]
        y, k = y + 1, k + 1
      end
      lo = hi
    end
    a, b = b, a
    width = width * 2
  end
  for k = 1, n do
    rawset(t, k, a[k])
  end
end


---------------------------------------------------------------------------
-- math: the functions the skins use, on numbers only

local math_lib = {}

function math_lib.floor(x)
  return native_floor(num(x, 1, "floor"))
end

function math_lib.ceil(x)
  return native_ceil(num(x, 1, "ceil"))
end

function math_lib.abs(x)
  return native_abs(num(x, 1, "abs"))
end

-- The largest (or smallest) argument, compared in Lua: the first of
-- equal ones, as Lua's own.
local function extreme(name, larger, ...)
  local n = native_select("#", ...)
  local args = { ... }
  local best = num(args[1], 1, name)
  for k = 2, n do
    local v = num(args[k], k, name)
    if larger(v, best) then
      best = v
    end
  end
  return best
end

function math_lib.max(...)
  return extreme("max", function(a, b) return a > b end, ...)
end

function math_lib.min(...)
  return extreme("min", function(a, b) return a < b end, ...)
end

---------------------------------------------------------------------------
-- base functions

local base = {}

-- The decimal and hexadecimal numerals Lua reads without a base: each
-- byte one a numeral may hold, checked in Lua before the short string is
-- read natively.
local NUMERAL = {}
do
  local chars = "0123456789abcdefABCDEFxXpP+-. \t\n\v\f\r"
  for i = 1, #chars do
    NUMERAL[byte(chars, i)] = true
  end
end

function base.tonumber(v, b)
  if b == nil then
    local t = type(v)
    if t == "number" then
      return v
    elseif t ~= "string" then
      if v == nil then
        arg_error(1, "tonumber", "value expected")
      end
      return nil
    end
    if #v > 200 then
      refuse(TONUMBER)
    end
    for i = 1, #v do
      if not NUMERAL[byte(v, i)] then
        return nil
      end
    end
    return native_tonumber(v)
  end
  -- Base 10 only, the base the skins use: digits, a sign and spaces.
  if mathtype(b) ~= "integer" or b ~= 10 then
    refuse(TONUMBER)
  end
  str(v, 1, "tonumber")
  local i, l = 1, #v
  while i <= l and is_space(byte(v, i)) do
    i = i + 1
  end
  local negative = false
  if byte(v, i) == 45 then
    negative = true
    i = i + 1
  elseif byte(v, i) == 43 then
    i = i + 1
  end
  local n, digits = 0, 0
  while i <= l do
    local c = byte(v, i)
    if c < 48 or c > 57 then
      break
    end
    n = n * 10 + (c - 48)
    digits = digits + 1
    i = i + 1
  end
  while i <= l and is_space(byte(v, i)) do
    i = i + 1
  end
  if digits == 0 or i <= l then
    return nil
  end
  return negative and -n or n
end

function base.select(n, ...)
  if n ~= "#" then
    n = int(n, 1, "select")
  end
  return native_select(n, ...)
end

function base.error(msg, level)
  level = int(level, 2, "error", 1)
  if level > 0 then
    level = level + 1
  end
  return native_error(msg, level)
end

-- The metatables this sandbox attached, each a snapshot it guards, and
-- the snapshot attached to each table; neither keeps a table alive.
local ours = native_setmetatable({}, { __mode = "k" })
local attached = native_setmetatable({}, { __mode = "k" })

-- A copy of the skin's metatable, made in Lua (its entries metered),
-- refused with `__gc` (a finalizer), `__mode` (weak references) or
-- `__close` (code run while an error unwinds), or with a `__name`
-- string longer than MAX_NAME.
local function snapshot(mt)
  local copy = {}
  for k, v in next, mt do
    rawset(copy, k, v)
  end
  if rawget(copy, "__gc") ~= nil or rawget(copy, "__mode") ~= nil
    or rawget(copy, "__close") ~= nil then
    refuse(FINALIZER)
  end
  local name = rawget(copy, "__name")
  if type(name) == "string" and #name > MAX_NAME then
    refuse(NAME)
  end
  if not bridge.metatable() then
    native_error(fault, 0)
  end
  return copy
end

function base.setmetatable(t, mt)
  if type(t) ~= "table" then
    arg_error(1, "setmetatable", "table expected, got " .. got(t))
  end
  if mt ~= nil and type(mt) ~= "table" then
    arg_error(2, "setmetatable", "nil or table expected")
  end
  local current = attached[t]
  if current ~= nil and not ours[current] then
    native_error("cannot change a protected metatable", 2)
  end
  if current == nil and native_getmetatable(t) ~= nil then
    native_error("cannot change a protected metatable", 2)
  end
  -- Every check is made before the guard is lifted.
  local copy = nil
  if mt ~= nil then
    copy = snapshot(mt)
    -- A metatable the skin did not protect itself names the skin's own
    -- table to getmetatable, and is guarded by the library.
    if rawget(mt, "__metatable") == nil then
      rawset(copy, "__metatable", mt)
      ours[copy] = true
    end
  end
  if current ~= nil then
    rawset(current, "__metatable", nil)
    ours[current] = nil
  end
  native_setmetatable(t, copy)
  attached[t] = copy
  return t
end

-- A bridge function taking one string: its argument checked here, so the
-- host never raises an error.
function base.string_bridge(f, name)
  return function(v)
    if type(v) ~= "string" then
      arg_error(1, name, "string expected, got " .. got(v))
    end
    if #v > max_path then
      refuse(PATH)
    end
    return f(v)
  end
end

-- ipairs, its step in Lua: the index is checked to be an integer, never
-- read from a string natively.
local function ipairs_step(t, i)
  if mathtype(i) ~= "integer" then
    arg_error(2, "ipairs", "integer expected, got " .. (mathtype(i) or got(i)))
  end
  i = i + 1
  local v = t[i]
  if v == nil then
    return nil
  end
  return i, v
end

function base.ipairs(t)
  if t == nil then
    arg_error(1, "ipairs", "table expected, got no value")
  end
  return ipairs_step, t, 0
end

return string_lib, table_lib, math_lib, base
