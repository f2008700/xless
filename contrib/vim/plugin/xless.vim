" xless.vim — open the current (or a given) XML file in xless, from
" inside vim/neovim, focused at the cursor's line. See the main README's
" "Vim integration" section for the full design and rationale; this is
" the "shipped plugin" §3 describes.
"
" Works in both vim 8+ (has('terminal')) and neovim (has('nvim')), using
" whichever :terminal implementation is available — both give xless a
" real pty, so it behaves exactly like running it directly in a shell
" (README.md's Vim integration section, §1).

if exists('g:loaded_xless')
  finish
endif
let g:loaded_xless = 1

if !exists('g:xless_command')
  let g:xless_command = 'xless'
endif

" :Xless             -> current buffer's file, focused at the cursor line
" :Xless {file}       -> explicit file, no focus line
function! xless#open(file) abort
  let l:line = line('.')

  if !empty(a:file)
    let l:target = a:file
    let l:focus_arg = ''
  else
    if empty(expand('%'))
      echoerr 'xless: current buffer has no associated file'
      return
    endif

    let l:target = expand('%:p')

    " If the buffer has unsaved changes, view a scratch copy instead of
    " the stale on-disk version — but make it read-only-in-spirit: xless
    " itself can edit and save now (README.md's Editing model section), so
    " silently letting :w inside that session overwrite a throwaway temp
    " file would be confusing. We just label it clearly in the message;
    " xless's own status bar will show the temp path, not your real
    " filename, as a visible reminder. See README.md's Editing model
    " section §6 / Vim integration section §5.
    if &modified
      let l:target = tempname() . '.xml'
      execute 'write ' . fnameescape(l:target)
      echom 'xless: viewing unsaved changes via a scratch copy at ' . l:target
            \ . ' — edits made inside xless will NOT be reflected back in this buffer'
    endif

    let l:focus_arg = ' --focus-line=' . l:line
  endif

  let l:cmd = g:xless_command . l:focus_arg . ' ' . fnameescape(l:target)

  if has('nvim')
    vsplit
    enew
    call termopen(l:cmd)
    startinsert
  elseif has('terminal')
    execute 'vertical terminal ' . l:cmd
  else
    " No :terminal support at all (very old vim) — fall back to
    " suspend-and-run, still works per VIM_INTEGRATION.md §1.
    execute '!' . l:cmd
  endif
endfunction

command! -nargs=? -complete=file Xless call xless#open(<q-args>)
