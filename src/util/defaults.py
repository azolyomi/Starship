import emoji

DEFAULT_VCLESS_CATEGORIES = ["oryx3", "shatters", "void", "cult", "fungal", "nest", "realmclear", "misc"]
DEFAULT_VCLESS_DESCRIPTION = """
This is a **VCLESS** run. React below with what you will bring to the run. 

**Fake reactions will be punished with an automatic timeout.** Do not react if you are not **absolutely certain** that you can bring the item to the run.
"""

DEFAULT_VCLESS_CATEGORY = {
    "organizer_roles": [],
    "channel_id": "",
    "message": {
        "title": "A run is brewing...",
        "description": DEFAULT_VCLESS_DESCRIPTION,
        "color": "",
        "thumbnail": "",
        "image": "",
    },
    "reactions": {},
},

REACTION_TEMPLATE = {
    "display": "Run Participant",
    "emoji": emoji.check,
    "num_required": 10,
    "confirm": False,
}


DEFAULT_SERVERCONFIG = {
        "guildID": "",
        "modroles": [],
        "adminroles": [],
        "staffroles": [],
        "log_channel_id": "",
        "raiding": {
            "vcless": {
                "control": {
                    "channel_id": "",
                    "message_id": ""
                },
                "categories": {
                    "oryx3": {
                        "emoji": emoji.oryx_3,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "An Oryx 3 run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Oryx 3 Participant",
                                "emoji": emoji.oryx_3,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "helmet_rune": {
                                "display": "Helmet Rune",
                                "num_required": 1,
                                "emoji": emoji.helm_rune,
                                "confirm": True,
                            },
                            "sword_rune": {
                                "display": "Sword Rune",
                                "num_required": 1,
                                "emoji": emoji.sword_rune,
                                "confirm": True,
                            },
                            "shield_rune": {
                                "display": "Shield Rune",
                                "num_required": 1,
                                "emoji": emoji.shield_rune,
                                "confirm": True,
                            }
                        },
                    },
                    "shatters": {
                        "emoji": emoji.shatters,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Shatters run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Shatters Participant",
                                "emoji": emoji.shatters,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "key": {
                                "display": "Shatters Key",
                                "emoji": emoji.shatters_key,
                                "num_required": 1,
                                "confirm": True
                            },
                        },
                    },
                    "void": {
                        "emoji": emoji.void,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Void run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Void Participant",
                                "emoji": emoji.void,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "key": {
                                "display": "Lost Halls Key",
                                "emoji": emoji.lost_halls_key,
                                "num_required": 1,
                                "confirm": True
                            },
                            "vial": {
                                "display": "Vial of Darkness",
                                "emoji": emoji.vial,
                                "num_required": 1,
                                "confirm": True
                            },
                        },
                    },
                    "cult": {
                        "emoji": emoji.cult,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Cultist Hideout run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Cultist Hideout Participant",
                                "emoji": emoji.cult,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "key": {
                                "display": "Lost Halls Key",
                                "emoji": emoji.lost_halls_key,
                                "num_required": 1,
                                "confirm": True
                            },
                        },
                    },
                    "fungal": {
                        "emoji": emoji.fungal,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Fungal Cavern run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Fungal Cavern Participant",
                                "emoji": emoji.fungal,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "key": {
                                "display": "Fungal Cavern Key",
                                "emoji": emoji.fungal_key,
                                "num_required": 1,
                                "confirm": True
                            },
                        },
                    },
                    "nest": {
                        "emoji": emoji.nest,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Nest run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Nest Participant",
                                "emoji": emoji.nest,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "key": {
                                "display": "Nest Key",
                                "emoji": emoji.nest_key,
                                "num_required": 1,
                                "confirm": True
                            },
                        }
                    },
                    "realmclear": {
                        "emoji": emoji.whitebag,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Realm Clearing session is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Realm Clearing Participant",
                                "emoji": emoji.whitebag,
                                "num_required": 10,
                                "confirm": False,
                            },
                        }
                    },
                    "misc": {
                        "emoji": emoji.nexus,
                        "organizer_roles": [],
                        "channel_id": "",
                        "message": {
                            "title": "A Miscellaneous run is brewing...",
                            "description": DEFAULT_VCLESS_DESCRIPTION,
                            "color": "",
                            "thumbnail": "",
                            "image": "",
                        },
                        "reactions": {
                            "interest": {
                                "display": "Miscellaneous Participant",
                                "emoji": emoji.nexus,
                                "num_required": 10,
                                "confirm": False,
                            },
                            "key": {
                                "display": "Miscellaneous Key",
                                "emoji": emoji.legendary_key,
                                "num_required": 1,
                                "confirm": True,
                            },
                        },
                    },
                }
            }
        }
    }