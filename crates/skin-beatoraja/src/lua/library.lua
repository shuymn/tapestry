-- The sandbox's string and table library (library.rs): every loop whose
-- length a skin chooses runs here, as Lua, so the instruction hook meters
-- it and sees cancellation. The native functions it keeps are called only
-- with validated arguments and a result size charged first (charge), and
-- reach no skin: they are this chunk's upvalues. An unsupported form is
-- refused (refuse), never approximated.
local native, charge, refuse, snapshot, cap = ...

local byte, sub, len = native.byte, native.sub, native.len
local native_char, native_rep = native.char, native.rep
local native_upper, native_lower, native_reverse = native.upper, native.lower, native.reverse
local native_format, native_concat, native_unpack = native.format, native.concat, native.unpack
local native_tonumber, native_tostring = native.tonumber, native.tostring
local native_setmetatable, native_getmetatable = native.setmetatable, native.getmetatable
local tointeger, mathtype, maxinteger = native.tointeger, native.mathtype, native.maxinteger
local type, error, select, rawget, rawset, rawlen = type, error, select, rawget, rawset, rawlen

-- The refusal codes library.rs names.
local PATTERN_BALANCE, PATTERN_FRONTIER, PATTERN_BACKREF = 1, 2, 3
local METATABLE, TOO_LARGE, FORMAT = 4, 5, 6

local function arg_error(n, fname, msg)
  error("bad argument #" .. n .. " to '" .. fname .. "' (" .. msg .. ")", 3)
end

-- Argument n as Lua's checklstring reads it: a string, or a number as its
-- text.
local function str(v, n, fname)
  local t = type(v)
  if t == "string" then
    return v
  elseif t == "number" then
    return native_tostring(v)
  end
  arg_error(n, fname, "string expected, got " .. (v == nil and "no value" or t))
end

-- Argument n as Lua's checkinteger reads it: an integer, a float with an
-- integral value, or a short numeric string.
local function int(v, n, fname, default)
  if v == nil and default ~= nil then
    return default
  end
  local t = type(v)
  if t == "string" and len(v) <= 64 then
    v = native_tonumber(v)
    t = type(v)
  end
  if t == "number" then
    local i = tointeger(v)
    if i then
      return i
    end
    arg_error(n, fname, "number has no integer representation")
  end
  arg_error(n, fname, "number expected, got " .. (v == nil and "no value" or t))
end

-- A table these functions read raw: one with a metatable, which Lua's
-- own functions would consult, is refused.
local function plain(t, n, fname)
  if type(t) ~= "table" then
    arg_error(n, fname, "table expected, got " .. (t == nil and "no value" or type(t)))
  end
  if native_getmetatable(t) ~= nil then
    refuse(METATABLE)
  end
  return t
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

string_lib.len = len

function string_lib.sub(s, i, j)
  s = str(s, 1, "sub")
  local l = len(s)
  i = start_of(int(i, 2, "sub", 1), l)
  j = end_of(int(j, 3, "sub", -1), l)
  if i <= j then
    bytes(j - i + 1)
  end
  return sub(s, i, j)
end

function string_lib.upper(s)
  s = str(s, 1, "upper")
  bytes(len(s))
  return native_upper(s)
end

function string_lib.lower(s)
  s = str(s, 1, "lower")
  bytes(len(s))
  return native_lower(s)
end

function string_lib.reverse(s)
  s = str(s, 1, "reverse")
  bytes(len(s))
  return native_reverse(s)
end

function string_lib.rep(s, n, sep)
  s = str(s, 1, "rep")
  n = int(n, 2, "rep")
  sep = sep == nil and "" or str(sep, 3, "rep")
  if n <= 0 then
    return ""
  end
  local each = len(s) + len(sep)
  if each > 0 and n > cap // each + 1 then
    refuse(TOO_LARGE)
  end
  bytes(n * len(s) + (n - 1) * len(sep))
  return native_rep(s, n, sep)
end

function string_lib.byte(s, i, j)
  s = str(s, 1, "byte")
  local l = len(s)
  local pi = int(i, 2, "byte", 1)
  i = start_of(pi, l)
  j = end_of(int(j, 3, "byte", pi), l)
  if i <= j then
    bytes(j - i + 1)
  end
  return byte(s, i, j)
end

function string_lib.char(...)
  local n = select("#", ...)
  bytes(n)
  return native_char(...)
end

-- The conversions string.format takes, by their letter: a number's text
-- is at most 99 width plus its digits; a string's is its length.
local NUMERIC = { d = 1, i = 1, u = 1, c = 1, x = 1, X = 1, o = 1, e = 1, E = 1, f = 1, F = 1,
  g = 1, G = 1, a = 1, A = 1 }

function string_lib.format(fmt, ...)
  fmt = str(fmt, 1, "format")
  local args = { ... }
  local nargs = select("#", ...)
  local total, argi, i, l = len(fmt), 0, 1, len(fmt)
  while i <= l do
    local c = byte(fmt, i)
    if c == 37 then
      i = i + 1
      if byte(fmt, i) == 37 then
        i = i + 1
      else
        local width = 0
        while true do
          c = byte(fmt, i)
          if c == nil then
            break
          end
          if c >= 48 and c <= 57 then
            width = width * 10 + (c - 48)
          elseif c ~= 45 and c ~= 43 and c ~= 32 and c ~= 35 and c ~= 46 then
            break
          end
          i = i + 1
        end
        if c == nil then
          refuse(FORMAT)
        end
        local letter = sub(fmt, i, i)
        i = i + 1
        argi = argi + 1
        if argi > nargs then
          arg_error(argi + 1, "format", "no value")
        end
        if letter == "s" then
          -- Converted once here, so the native call reads no metamethod.
          local v = args[argi]
          if type(v) ~= "string" then
            v = native_tostring(v)
            args[argi] = v
          end
          total = total + len(v) + width
        elseif letter == "q" then
          local v = args[argi]
          total = total + 2 * (type(v) == "string" and len(v) or 32) + 2
        elseif NUMERIC[letter] then
          total = total + width + 330
        else
          refuse(FORMAT)
        end
      end
    else
      i = i + 1
    end
  end
  bytes(total)
  return native_format(fmt, native_unpack(args, 1, nargs))
end

---------------------------------------------------------------------------
-- patterns: one matcher, Lua 5.4's (lstrlib.c), for find, match, gmatch
-- and gsub. %b, %f and back references are refused.

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

local function prepare(s, p)
  M.s, M.sl, M.p, M.pl = s, len(s), p, len(p)
  M.init, M.len = {}, {}
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
  return native_unpack(out, 1, n)
end

-- Whether a pattern has none of the special characters.
local function no_specials(p)
  for i = 1, len(p) do
    local c = byte(p, i)
    if c == 94 or c == 36 or c == 42 or c == 43 or c == 63 or c == 46 or c == 40
      or c == 41 or c == 91 or c == 93 or c == 37 or c == 45 then
      return false
    end
  end
  return true
end

local function plain_find(s, p, init)
  local sl, pl = len(s), len(p)
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
  local sl = len(s)
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

-- A leading `^` is a literal in gmatch, as in Lua 5.4; an exhausted
-- iterator stays exhausted.
function string_lib.gmatch(s, p, init)
  s = str(s, 1, "gmatch")
  p = str(p, 2, "gmatch")
  local sl = len(s)
  local pos = start_of(int(init, 3, "gmatch", 1), sl)
  local last = nil
  local done = false
  return function()
    if done then
      return nil
    end
    prepare(s, p)
    local si = pos
    while si <= sl + 1 do
      reset()
      local e = do_match(si, 1)
      if e and e ~= last then
        pos, last = e, e
        return captures(si, e, true)
      end
      si = si + 1
    end
    done = true
    return nil
  end
end

-- The text of a gsub replacement for a match from si to e.
local function replacement(repl, kind, si, e, out)
  if kind == "string" then
    local rl = len(repl)
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
    v = repl(captures(si, e, true))
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
    total = total + len(out[k])
  end
  bytes(total + len(sep) * (#out > 0 and #out - 1 or 0))
  return native_concat(out, sep)
end

function string_lib.gsub(s, p, repl, max_n)
  s = str(s, 1, "gsub")
  p = str(p, 2, "gsub")
  local kind = type(repl)
  if kind == "number" then
    repl = native_tostring(repl)
    kind = "string"
  end
  if kind == "table" then
    plain(repl, 3, "gsub")
  elseif kind ~= "string" and kind ~= "function" then
    arg_error(3, "gsub", "string/function/table expected, got " .. kind)
  end
  local sl = len(s)
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
      prepare(s, p)
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

function table_lib.move(a1, f, e, t, a2)
  plain(a1, 1, "move")
  f = int(f, 2, "move")
  e = int(e, 3, "move")
  t = int(t, 4, "move")
  if a2 == nil then
    a2 = a1
  else
    plain(a2, 5, "move")
  end
  if e >= f then
    if not (f > 0 or e < maxinteger + f) then
      arg_error(3, "move", "too many elements to move")
    end
    local n = e - f
    if t > maxinteger - n then
      arg_error(4, "move", "destination wrap around")
    end
    if t > e or t <= f or a1 ~= a2 then
      for i = 0, n do
        rawset(a2, t + i, rawget(a1, f + i))
      end
    else
      for i = n, 0, -1 do
        rawset(a2, t + i, rawget(a1, f + i))
      end
    end
  end
  return a2
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

function table_lib.unpack(t, i, j)
  plain(t, 1, "unpack")
  i = int(i, 2, "unpack", 1)
  j = int(j, 3, "unpack", rawlen(t))
  if i > j then
    return
  end
  if j - i >= 1000000 then
    error("too many results to unpack", 2)
  end
  local out = {}
  for k = i, j do
    out[k - i + 1] = rawget(t, k)
  end
  return native_unpack(out, 1, j - i + 1)
end

function table_lib.pack(...)
  local t = { ... }
  t.n = select("#", ...)
  return t
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
-- base functions

local function tonumber(v, base)
  if type(v) == "string" then
    bytes(len(v))
  end
  if base == nil then
    return native_tonumber(v)
  end
  return native_tonumber(v, base)
end

-- The metatables this sandbox attached, each a snapshot it guards, and
-- the snapshot attached to each table.
local ours, attached = {}, {}

local function setmetatable(t, mt)
  if type(t) ~= "table" then
    arg_error(1, "setmetatable", "table expected, got " .. (t == nil and "no value" or type(t)))
  end
  if mt ~= nil and type(mt) ~= "table" then
    arg_error(2, "setmetatable", "nil or table expected")
  end
  local current = attached[t]
  if current ~= nil and not ours[current] then
    error("cannot change a protected metatable", 2)
  end
  if current == nil and native_getmetatable(t) ~= nil then
    -- A metatable attached outside this function: only the sandbox's own.
    error("cannot change a protected metatable", 2)
  end
  -- Every check is made before the guard is lifted.
  local copy = nil
  if mt ~= nil then
    copy = snapshot(mt)
  end
  if current ~= nil then
    rawset(current, "__metatable", nil)
  end
  native_setmetatable(t, copy)
  if copy ~= nil then
    attached[t] = copy
    if rawget(copy, "__metatable") == mt then
      ours[copy] = true
    end
  else
    attached[t] = nil
  end
  return t
end

return string_lib, table_lib, tonumber, setmetatable
