from discord.ext.commands import CommandError

# Global Error setup 
class GlobalError(CommandError):
    def __init__(self, error):
        self.original = error

def propagate(ctx, error):
    ctx.bot.dispatch('command_error', ctx, GlobalError(error))

class StarshipPermissionsError(CommandError):
    message = "You do not have permission to use this command."

class NotModError(StarshipPermissionsError):
    message = "You must have a `mod` role configured with the bot use this command."

class NotAdminError(StarshipPermissionsError):
    message = "You must have an `admin` role configured with the bot use this command."

class NotStaffError(StarshipPermissionsError):
    message = "You must have a `staff` role configured with the bot use this command."
