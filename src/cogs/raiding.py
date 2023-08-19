import discord
from discord.ext import commands
from typing import List, Dict
from emoji import check
from util import logger
from permissions.checks import has_staff_role
from database import ServerConfigs
from util.converters import ExistingCategory
from ui.views.confirm import Confirm

class Reaction:
    display = ""
    emoji = ""
    num_required = 0
    reacted = set()
    confirm = False
    def __init__(self, reaction_as_dict: Dict):
        self.display = reaction_as_dict.get('display')
        self.emoji = reaction_as_dict.get('emoji')
        self.num_required = reaction_as_dict.get('num_required')
        self.confirm = reaction_as_dict.get('confirm')

class ReactionMenu(discord.ui.View):
    def __init__(self, reactions: List[Reaction]):
        super().__init__(timeout=None)
        self.reactions = reactions
        # self.startable = all(len(reaction.reacted) == reaction.num_required for reaction in reactions)
        for reaction in reactions:
            self.add_item(ReactionButton(reaction))
        self.add_item(StartButton(disabled = not self.startable()))
    
    def startable(self):
        return all(len(reaction.reacted) >= reaction.num_required for reaction in self.reactions)


class ReactionButton(discord.ui.Button):
    def __init__(self, reaction: Reaction):
        self.reaction = reaction
        super().__init__(custom_id=f"RAID_{reaction.display}", label=reaction.display, emoji=reaction.emoji, style=discord.ButtonStyle.grey)
        
    
    async def callback(self, interaction: discord.Interaction):
        self.reaction.reacted.add(interaction.user.id)
        await interaction.response.send_message(embed=discord.Embed(title="Confirm", description="?"), view = Confirm())
        if (self.view.startable()):
            start_button = discord.utils.get(self.view.children, custom_id="START")
            start_button.disabled = False
        # await interaction.followup.edit_message(view=self.view)
        # do stuff here
        return

class StartButton(discord.ui.Button):
    def __init__(self, disabled):
        super().__init__(custom_id = "START", style = discord.ButtonStyle.green, label="Start", disabled=disabled)

    async def callback(self, interaction: discord.Interaction):
        await interaction.response.defer()
        print("start clicked")
        # do stuff here
        return

class Raiding(commands.Cog):
    def __init__(self, bot: commands.Bot):
        self.bot = bot

    @commands.command(pass_context=True, aliases=['raid'], usage="<category> <location>")
    @commands.check(has_staff_role)
    @commands.guild_only()
    async def afk(self, ctx, category_name: ExistingCategory, location):
        category = ServerConfigs[ctx.guild.id]["raiding"]["categories"][category_name]
        reactions = list(map(lambda reaction: Reaction(reaction), category["reactions"].values()))
        menu = ReactionMenu(reactions)
        message = category["message"]
        embed = discord.Embed(title=message["title"], description=message["description"])
        if (len(message["thumbnail"]) > 0): embed.set_thumbnail(message["thumbnail"])
        if (len(message["image"]) > 0): embed.set_image(icon=message["image"])
        await ctx.channel.send(embed=embed, view=menu)



async def setup(bot):
    await bot.add_cog(Raiding(bot))