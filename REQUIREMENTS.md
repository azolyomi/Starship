This is a discord bot whose main goal is to facilitate "raids" in a popular game Realm of the Mad God. 
Our goal here today is going to be to effectively rewrite this entire thing and make it work how I want. 
Everything is up for debate -- what we use for persistent storage, what language we write it in, everything.

# Raiding
A "raid" is an organized run of a dungeon in game. These dungeons can be accessed one of two ways: either an item is required to start the dungeon, e.g., a "key", or the dungeon can be found naturally. 
Different dungeons have different requirements. Every dungeon can be found naturally, but that often takes a while -- so we want to also provide the ability to check if anyone has a key to open the dungeon.
Some dungeons have a required list of items to open the dungeon and use it. 

A "headcount" is a rich message in discord, sent by this bot, which has a few major reactions:
1. A reaction of some emoji for the average player to indicate they are willing to participate in the run as a raider.
2. A reaction of any required items for this raid, usually either a key (to save time) or other required items to actually open the dungeon.

A headcount may be turned into a "run" by people with correct permissions, once all the requirements are met. The person to do this is designated as the "raid leader", and it will usually be the person who created the headcount.
A run is another rich message in discord, sent by this bot, which tracks this group's dungeon running. It can be multiple runs in a row (chained). 
The run provides a few pieces of context:
1. What dungeon the raid is for
2. Who initiated it (tag them @...)
3. Where the run is (location) (optional, specified by raid leader)
4. What party to join the run at (optional, specified by raid leader)

**Control panel**:
Each run must have a control panel that is visible ONLY to the leader of that run. 
This control panel must offer the ability to change any mutable run properties (which will in turn propagate to the run message).
This includes:
- location
- party
- run owner (transfer ownership)
When a run is finished, the raid leader will interface with the bot control panel to end the run. This will edit the run message to a "terminated" state and close out the run. 

# Kinds of raids
The majority of raids will be "vcless" raids. These are the above kind -- they are started asynchronously, and are organized by a real human but there is no live instruction. Players
However, there are exceptions to this rule. Occasionally, a dungeon is hard enough that we will require users to join a voice channel, and will provide verbal instruction in that channel. These are constituted as "vc" raids. 

There is also a notion of different "tiers" of raids. For example, I might have a dedicated section of my server reserved for incredibly skilled players. In this case, I might want to have an entire copy of this system for this subsection. 
The key here: everything must be configurable. 


# Requirements
1. The bot must facilitate the above raiding process.
2. Everything must be configurable. Want to have multiple types of raids associated with one dungeon? Done. Want to use different colors? Done. Want to put certain run types in a different channel? Done. (you can guarantee one run type is mapped to exactly one channel). 

# Other Requirements
The bot MUST be easy to set up in a new server. This includes:
1. Onboarding configuration of permissions for roles (who do you want to have access to what things?)
2. Onboarding configuration of channel messages (what channels do they go to?)

The bot must have an internal, persistent storage for a server's configuration. This includes channels the bot has created/is configured to use, role IDs it has made (or been configured to use) to associate with permissions in that server, etc.
The bot will have a fully fledged permission system to enable configurable permissions. 
The developer will have superadmin -- this is one account that is by default given every permission on every server. This will be a userid. 
The bot must allow the user to specify already existing roles to tie to those permissions, and optionally create them as a default. 
The bot will potentially be supporting O(1000) concurrent runs across O(10) servers. We will start with one.
The bot must have 100% uptime. Reliability is first -- this will be running 24/7 and people from all over the world will be relying on it.

