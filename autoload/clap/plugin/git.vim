" Author: liuchengxu <xuliuchengxlc@gmail.com>

scriptencoding utf-8

let s:save_cpo = &cpoptions
set cpoptions&vim

highlight default link ClapBlameInfo SpecialComment

if has('nvim')
  function! clap#plugin#git#clear_blame_info(bufnr) abort
    let id = getbufvar(a:bufnr, 'clap_git_blame_extmark_id')
    if !empty(id)
      call nvim_buf_del_extmark(a:bufnr, s:blame_ns_id, id)
    endif
  endfunction

  function! clap#plugin#git#show_cursor_blame_info(bufnr, text) abort
    if !exists('s:blame_ns_id')
      let s:blame_ns_id = nvim_create_namespace('clap_blame')
    endif

    let id = getbufvar(a:bufnr, 'clap_git_blame_extmark_id')
    if !empty(id)
      call nvim_buf_del_extmark(a:bufnr, s:blame_ns_id, id)
    endif

    let always_eol = v:true
    let available_space = winwidth(bufwinid(a:bufnr)) - col('$')
    if always_eol || available_space > strlen(a:text)
      let opts = { 'virt_text': [[a:text, 'ClapBlameInfo']], 'virt_text_pos': 'eol' }
    else
      let text = &signcolumn ==# 'yes' ? printf('  ╰─▸ %s', a:text) : a:text
      let text = &numberwidth > 0 ? printf('%s%s', repeat(' ', &numberwidth/2), text) : text
      let opts = { 'virt_lines': [[[text, 'ClapBlameInfo']]], 'virt_lines_leftcol': col('.')  - 1 }
    endif

    try
      let last_id = nvim_buf_set_extmark(a:bufnr, s:blame_ns_id, line('.') - 1, col('.') - 1, opts)
      call setbufvar(a:bufnr, 'clap_git_blame_extmark_id', last_id)
    " Suppress error: Invalid 'col': out of range
    catch /^Vim\%((\a\+)\)\=:E5555/
    endtry
  endfunction

else

  function! clap#plugin#git#clear_blame_info(bufnr) abort
    " Popup will be closed automatically due to the `moved` option.
  endfunction

  function! clap#plugin#git#show_cursor_blame_info(bufnr, text) abort
    let col_offset = 4 + col('$') - col('.')
    let popup_id = popup_create(a:text, {
          \ 'line': 'cursor',
          \ 'col': printf('cursor+%d', col_offset),
          \ 'highlight': 'ClapBlameInfo',
          \ 'wrap': v:true,
          \ 'zindex': 100,
          \ 'moved': [line('.'), 1, -1],
          \ })
  endfunction

endif

let &cpoptions = s:save_cpo
unlet s:save_cpo
