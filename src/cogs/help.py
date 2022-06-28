from pretty_help import PrettyHelp, DefaultMenu

def setup(bot): 
    # setup pretty help
    menu = DefaultMenu(delete_after_timeout=True, remove="❌")
    bot.help_command=PrettyHelp(menu=menu, color=0x062070)